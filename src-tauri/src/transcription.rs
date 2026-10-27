//! Batch request format follows the user's Handy Codex integration; see THIRD_PARTY_NOTICES.md.
use crate::codex::Rpc;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
    fs,
    io::Cursor,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
const MAX_SAMPLES: usize = 24_000 * 300;
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static REFRESH: Mutex<()> = Mutex::new(());
struct Credentials {
    token: String,
    account: Option<String>,
    expires: u64,
}
fn parse_credentials(value: &Value) -> Result<Credentials, String> {
    let token = value["tokens"]["access_token"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .ok_or("Не найден вход ChatGPT в Codex. Выполните codex login.")?
        .to_string();
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let account = claims
        .as_ref()
        .and_then(|v| v["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| value["tokens"]["account_id"].as_str())
        .map(str::to_string);
    let expires = claims.as_ref().and_then(|c| c["exp"].as_u64()).unwrap_or(0);
    Ok(Credentials {
        token,
        account,
        expires,
    })
}
fn read_credentials() -> Result<Credentials, String> {
    let path = crate::platform::codex_auth_path()?;
    let bytes = fs::read(path).map_err(|_| {
        "Не удалось прочитать вход Codex. Войдите в Codex через ChatGPT.".to_string()
    })?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Файл входа Codex повреждён.".to_string())?;
    parse_credentials(&value)
}
pub fn credentials_available() -> bool {
    read_credentials().is_ok()
}
fn fresh_credentials(previous: Option<&str>) -> Result<Credentials, String> {
    let _guard = REFRESH.lock().map_err(|e| e.to_string())?;
    let current = read_credentials()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    if previous.is_some_and(|old| old != current.token)
        || previous.is_none() && current.expires > now + 300
    {
        return Ok(current);
    }
    // Let Codex own refresh-token rotation and its shared credential store.
    let rpc = Rpc::open()?;
    rpc.request("account/read", json!({"refreshToken":true}))?;
    read_credentials()
}
fn multipart(wav: &[u8], boundary: &str) -> Vec<u8> {
    let mut body=format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"codex.wav\"\r\nContent-Type: audio/wav\r\n\r\n").into_bytes();
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}
fn post(
    client: &Client,
    wav: &[u8],
    auth: &Credentials,
) -> Result<reqwest::blocking::Response, String> {
    let boundary = format!(
        "----codex-transcribe-{}-{}",
        crate::session::now_ms(),
        REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let mut request = client
        .post(ENDPOINT)
        .bearer_auth(&auth.token)
        .header("originator", "Codex Desktop")
        .header("User-Agent", crate::platform::transcription_user_agent())
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(multipart(wav, &boundary));
    if let Some(account) = &auth.account {
        request = request.header("ChatGPT-Account-Id", account);
    }
    request
        .send()
        .map_err(|e| format!("Не удалось отправить отрезок в Codex: {e}"))
}
fn request_text(client: &Client, wav: &[u8]) -> Result<String, String> {
    let auth = fresh_credentials(None)?;
    let mut response = post(client, wav, &auth)?;
    if response.status().as_u16() == 401 {
        let fresh = fresh_credentials(Some(&auth.token))?;
        response = post(client, wav, &fresh)?;
    }
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "Вход Codex истёк. Откройте Codex, обновите вход и повторите отрезок.".into(),
            429 => "Достигнут лимит распознавания Codex. Аудио сохранено; повторите позже.".into(),
            413 => "Codex отклонил размер аудиозапроса. Аудио сохранено.".into(),
            _ => format!(
                "Codex не распознал отрезок: HTTP {}. Аудио сохранено; можно повторить.",
                status.as_u16()
            ),
        });
    }
    let value: Value = response
        .json()
        .map_err(|_| "Codex вернул некорректный ответ на распознавание.".to_string())?;
    value["text"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or("В ответе Codex нет текста расшифровки.".into())
}
fn quiet_boundary(samples: &[i16], sample_rate: u32) -> usize {
    let window = (sample_rate as usize / 10).max(1);
    let start = samples.len().saturating_sub(sample_rate as usize * 2);
    samples[start..]
        .chunks_exact(window)
        .enumerate()
        .min_by_key(|(_, part)| {
            part.iter()
                .map(|v| (*v as i32).unsigned_abs() as u64)
                .sum::<u64>()
        })
        .map(|(i, _)| start + i * window + window / 2)
        .filter(|i| *i > 0 && *i < samples.len())
        .unwrap_or(samples.len())
}
fn wav_bytes(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    let mut cursor = Cursor::new(Vec::with_capacity(samples.len() * 2 + 64));
    {
        let mut writer = hound::WavWriter::new(
            &mut cursor,
            hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .map_err(|e| e.to_string())?;
        for sample in samples {
            writer.write_sample(*sample).map_err(|e| e.to_string())?;
        }
        writer.finalize().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}
pub fn transcribe(path: &Path) -> Result<String, String> {
    let mut wav = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = wav.spec();
    if spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err("Ожидался WAV PCM 16 bit mono.".into());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let mut input = wav.samples::<i16>();
    let mut pending = vec![];
    let mut parts = vec![];
    loop {
        while pending.len() < MAX_SAMPLES {
            match input.next() {
                Some(sample) => pending.push(sample.map_err(|e| e.to_string())?),
                None => break,
            }
        }
        if pending.is_empty() {
            break;
        }
        let extra = input.next().transpose().map_err(|e| e.to_string())?;
        let boundary = if extra.is_some() {
            quiet_boundary(&pending, spec.sample_rate)
        } else {
            pending.len()
        };
        if pending[..boundary].iter().any(|sample| *sample != 0) {
            parts.push(request_text(
                &client,
                &wav_bytes(&pending[..boundary], spec.sample_rate)?,
            )?);
        }
        pending.drain(..boundary);
        if let Some(sample) = extra {
            pending.push(sample);
        }
    }
    Ok(parts
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn digital_silence_does_not_create_a_cloud_request_or_a_fake_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("silence.wav");
        fs::write(&path, wav_bytes(&[0; 480], 24000).unwrap()).unwrap();
        assert_eq!(transcribe(&path).unwrap(), "");
    }
    #[test]
    fn multipart_contains_a_complete_independent_wav() {
        let wav = wav_bytes(&[1, -2, 3], 24000).unwrap();
        let body = multipart(&wav, "test-boundary");
        assert!(body.starts_with(b"--test-boundary\r\n"));
        assert!(body.windows(wav.len()).any(|w| w == wav));
        assert!(body.ends_with(b"\r\n--test-boundary--\r\n"));
    }
    #[test]
    fn large_segment_boundaries_preserve_every_sample() {
        let mut samples = vec![1000_i16; 24000 * 4];
        samples[60000..62400].fill(0);
        let boundary = quiet_boundary(&samples, 24000);
        assert!((60000..62400).contains(&boundary));
        let joined: Vec<_> = samples[..boundary]
            .iter()
            .chain(&samples[boundary..])
            .copied()
            .collect();
        assert_eq!(joined, samples);
    }
    #[test]
    fn credentials_use_the_existing_codex_login() {
        let payload=URL_SAFE_NO_PAD.encode(br#"{"exp":9999999999,"https://api.openai.com/auth":{"chatgpt_account_id":"account-test"}}"#);
        let credentials = parse_credentials(
            &json!({"tokens":{"access_token":format!("header.{payload}.signature")}}),
        )
        .unwrap();
        assert_eq!(credentials.account.as_deref(), Some("account-test"));
        assert!(parse_credentials(&json!({"OPENAI_API_KEY":"not-a-chatgpt-login"})).is_err());
    }
}
