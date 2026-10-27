use crate::session::{private_dir, AnswerOptions, Settings, SAMPLE_RATE};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Devices {
    pub microphones: Vec<Device>,
    pub outputs: Vec<Device>,
    pub default_microphone: String,
    pub default_output: String,
}

fn command_text(args: &[&str]) -> Result<String, String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| format!("Не удалось запустить pactl: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
pub fn devices() -> Result<Devices, String> {
    let sources: Vec<serde_json::Value> =
        serde_json::from_str(&command_text(&["-f", "json", "list", "sources"])?)
            .map_err(|e| e.to_string())?;
    let mut microphones = vec![];
    let mut outputs = vec![];
    for source in sources {
        let name = source["name"]
            .as_str()
            .ok_or("У аудиоустройства нет имени.")?
            .to_string();
        let description = device_description(&source, &name);
        if name.ends_with(".monitor") {
            outputs.push(Device { name, description });
        } else {
            microphones.push(Device { name, description });
        }
    }
    Ok(Devices {
        microphones,
        outputs,
        default_microphone: command_text(&["get-default-source"])?,
        default_output: format!("{}.monitor", command_text(&["get-default-sink"])?),
    })
}

fn device_description(source: &serde_json::Value, name: &str) -> String {
    [
        &source["description"],
        &source["properties"]["device.description"],
        &source["properties"]["node.nick"],
        &source["properties"]["device.nick"],
    ]
    .into_iter()
    .filter_map(|value| value.as_str().map(str::trim))
    .find(|text| {
        !text.is_empty()
            && !text.eq_ignore_ascii_case("(null)")
            && !text.eq_ignore_ascii_case("null")
    })
    .unwrap_or(name)
    .to_string()
}

pub struct WaveFile {
    file: File,
    pub samples: u64,
}
impl WaveFile {
    pub fn create(path: &Path) -> Result<Self, String> {
        private_dir(path.parent().ok_or("Нет каталога аудио.")?)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("Не удалось создать запись: {e}"))?;
        file.write_all(&wave_header(0)?)
            .map_err(|e| e.to_string())?;
        Ok(Self { file, samples: 0 })
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !bytes.len().is_multiple_of(2) {
            return Err("Неполный аудиосэмпл.".into());
        }
        let next = self.samples + bytes.len() as u64 / 2;
        wave_header(next)?;
        self.file
            .write_all(bytes)
            .map_err(|e| format!("Ошибка записи звука: {e}"))?;
        self.samples = next;
        Ok(())
    }
    pub fn checkpoint(&mut self) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| {
                self.file
                    .write_all(&wave_header(self.samples).map_err(std::io::Error::other)?)
            })
            .and_then(|_| self.file.seek(SeekFrom::End(0)))
            .and_then(|_| self.file.sync_data())
            .map_err(|e| format!("Не удалось сохранить заголовок аудио: {e}"))?;
        Ok(())
    }
}
fn wave_header(samples: u64) -> Result<[u8; 44], String> {
    // shortcut: PCM WAV is limited to 4 GiB per segment; use RF64 for day-long segments.
    let size = u32::try_from(samples.checked_mul(2).ok_or("Аудиофайл слишком большой.")?)
        .map_err(|_| "Отрезок превысил размер WAV; сохранённые данные остаются на диске.")?;
    let riff = size.checked_add(36).ok_or("Отрезок превысил размер WAV.")?;
    let mut h = [0; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff.to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&size.to_le_bytes());
    Ok(h)
}
pub fn recover_wave(path: &Path) -> Result<u64, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut h = [0; 44];
    file.read_exact(&mut h).map_err(|e| e.to_string())?;
    let expected = wave_header(0)?;
    if h[0..4] != expected[0..4] || h[8..40] != expected[8..40] {
        return Err("Неизвестный формат незавершённого аудио.".into());
    }
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let samples = (len.checked_sub(44).ok_or("Повреждён WAV.")?) / 2;
    file.set_len(44 + samples * 2)
        .and_then(|_| file.seek(SeekFrom::Start(0)))
        .and_then(|_| file.write_all(&wave_header(samples).map_err(std::io::Error::other)?))
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(samples)
}

pub enum CaptureEvent {
    Level {
        samples: u64,
        peak: f32,
    },
    Cut {
        seq: u64,
        end_sample: u64,
        request: Option<AnswerOptions>,
        continuing: bool,
    },
    Empty {
        request: AnswerOptions,
    },
    Failed(String),
    Stopped,
}
type Cut = Option<AnswerOptions>;
pub struct Capture {
    cuts: Sender<Cut>,
    child: Arc<Mutex<Child>>,
    stopping: Arc<AtomicBool>,
}
impl Capture {
    pub fn cut(&self, request: Option<AnswerOptions>) -> Result<(), String> {
        if self.stopping.load(Ordering::Acquire) {
            return Err("Запись уже останавливается.".into());
        }
        self.cuts
            .send(request)
            .map_err(|_| "Запись не работает.".into())
    }
    pub fn stop(&self) -> Result<(), String> {
        if !self.stopping.swap(true, Ordering::AcqRel) {
            let pid = self.child.lock().map_err(|e| e.to_string())?.id();
            if unsafe { libc::kill(pid as i32, libc::SIGINT) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn start(
    directory: PathBuf,
    settings: Settings,
    seq: u64,
    start_sample: u64,
    events: Sender<CaptureEvent>,
) -> Result<Capture, String> {
    let available = devices()?;
    if !available
        .microphones
        .iter()
        .any(|d| d.name == settings.microphone)
        || !available.outputs.iter().any(|d| d.name == settings.output)
    {
        return Err("Выберите доступные микрофон и системный выход.".into());
    }
    let first = WaveFile::create(&directory.join(format!("audio/{seq:06}.wav")))?;
    let mut command = Command::new("ffmpeg");
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-thread_queue_size",
        "512",
        "-f",
        "pulse",
        "-fragment_size",
        "4096",
        "-i",
        &settings.output,
        "-thread_queue_size",
        "512",
        "-f",
        "pulse",
        "-fragment_size",
        "4096",
        "-i",
        &settings.microphone,
        "-filter_complex",
        "amix=inputs=2:duration=longest:dropout_transition=0:normalize=1",
        "-ac",
        "1",
        "-ar",
        "24000",
        "-c:a",
        "pcm_s16le",
        "-f",
        "s16le",
        "pipe:1",
    ]);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Не удалось запустить ffmpeg: {e}"))?;
    let stdout = child.stdout.take().ok_or("Нет аудиопотока ffmpeg.")?;
    let stderr = child.stderr.take().ok_or("Нет канала ошибок ffmpeg.")?;
    let child = Arc::new(Mutex::new(child));
    let stopping = Arc::new(AtomicBool::new(false));
    let error_text = Arc::new(Mutex::new(String::new()));
    let stderr_text = error_text.clone();
    thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if let Ok(mut s) = stderr_text.lock() {
                s.push_str(&line);
                s.push('\n');
                if s.len() > 4000 {
                    let drain = s
                        .char_indices()
                        .find(|(i, _)| *i >= s.len() - 3000)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    s.drain(..drain);
                }
            }
        }
    });
    let (cuts_tx, cuts_rx) = mpsc::channel();
    let process = child.clone();
    let stop = stopping.clone();
    let last_audio = Arc::new(AtomicU64::new(crate::session::now_ms()));
    let watchdog_audio = last_audio.clone();
    let watchdog_stop = stop.clone();
    let watchdog_process = process.clone();
    let watchdog_events = events.clone();
    thread::spawn(move || {
        while !watchdog_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_secs(1));
            if crate::session::now_ms().saturating_sub(watchdog_audio.load(Ordering::Relaxed))
                > 15_000
            {
                let _ = watchdog_events.send(CaptureEvent::Failed(
                    "Аудиоустройства не выдают данные более 15 секунд.".into(),
                ));
                if let Ok(mut child) = watchdog_process.lock() {
                    let _ = child.kill();
                }
                break;
            }
        }
    });
    thread::spawn(move || {
        let result = record(
            stdout,
            first,
            directory,
            seq,
            start_sample,
            cuts_rx,
            &events,
            &last_audio,
        );
        let intentional = stop.swap(true, Ordering::AcqRel);
        if let Err(message) = result {
            let _ = events.send(CaptureEvent::Failed(message));
            if let Ok(mut p) = process.lock() {
                let _ = p.kill();
            }
        }
        let status = process.lock().ok().and_then(|mut p| p.wait().ok());
        if !intentional && !status.is_some_and(|s| s.success()) {
            let message = error_text.lock().map(|s| s.clone()).unwrap_or_default();
            let _ = events.send(CaptureEvent::Failed(format!(
                "Захват звука завершился: {}",
                message.trim()
            )));
        }
        let _ = events.send(CaptureEvent::Stopped);
    });
    Ok(Capture {
        cuts: cuts_tx,
        child,
        stopping,
    })
}

#[allow(clippy::too_many_arguments)]
fn record(
    mut input: impl Read,
    mut file: WaveFile,
    directory: PathBuf,
    mut seq: u64,
    mut start: u64,
    cuts: Receiver<Cut>,
    events: &Sender<CaptureEvent>,
    last_audio: &AtomicU64,
) -> Result<(), String> {
    let mut bytes = [0; 8192];
    let mut carry = 0;
    let mut checkpoint = 0;
    let mut last_meter = 0;
    loop {
        let n = input.read(&mut bytes[carry..]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        last_audio.store(crate::session::now_ms(), Ordering::Relaxed);
        let length = n + carry;
        let even = length - length % 2;
        file.write(&bytes[..even])?;
        let total = start + file.samples;
        if total - last_meter >= 4800 {
            let peak = bytes[..even]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| (i16::from_le_bytes([s[0], s[1]]) as f32).abs() / 32768.0)
                .fold(0.0_f32, f32::max);
            let _ = events.send(CaptureEvent::Level {
                samples: total,
                peak,
            });
            last_meter = total;
        }
        carry = length % 2;
        if carry == 1 {
            bytes[0] = bytes[length - 1];
        }
        if file.samples - checkpoint >= SAMPLE_RATE as u64 {
            file.checkpoint()?;
            checkpoint = file.samples;
        }
        while let Ok(request) = cuts.try_recv() {
            if file.samples == 0 {
                if let Some(request) = request {
                    let _ = events.send(CaptureEvent::Empty { request });
                }
                continue;
            }
            let mut next = WaveFile::create(&directory.join(format!("audio/{:06}.wav", seq + 1)))?;
            file.checkpoint()?;
            let end = start + file.samples;
            std::mem::swap(&mut file, &mut next);
            drop(next);
            events
                .send(CaptureEvent::Cut {
                    seq,
                    end_sample: end,
                    request,
                    continuing: true,
                })
                .map_err(|_| "Сессия недоступна.")?;
            seq += 1;
            start = end;
            checkpoint = 0;
        }
    }
    let mut remaining = cuts.try_iter();
    file.checkpoint()?;
    if file.samples > 0 {
        let _ = events.send(CaptureEvent::Cut {
            seq,
            end_sample: start + file.samples,
            request: remaining.next().flatten(),
            continuing: false,
        });
    } else {
        drop(file);
        fs::remove_file(directory.join(format!("audio/{seq:06}.wav")))
            .map_err(|e| e.to_string())?;
    }
    for request in remaining.flatten() {
        let _ = events.send(CaptureEvent::Empty { request });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_labels_use_real_names_when_pactl_returns_null_strings() {
        let source = serde_json::json!({"description":"(null)","properties":{"device.description":"(null)","node.nick":"ALC887-VD Analog"}});
        assert_eq!(
            device_description(&source, "alsa_output.monitor"),
            "ALC887-VD Analog"
        );
        assert_eq!(
            device_description(&serde_json::json!({"description":null}), "device-id"),
            "device-id"
        );
        assert_eq!(
            device_description(
                &serde_json::json!({"description":" ","properties":{"node.nick":"null"}}),
                "device-id"
            ),
            "device-id"
        );
        assert_eq!(
            device_description(
                &serde_json::json!({"description":"Мой микрофон"}),
                "device-id"
            ),
            "Мой микрофон"
        );
    }
    #[test]
    fn answer_request_queued_at_stop_is_not_lost() {
        struct EndingReader {
            sent: bool,
            cuts: Sender<Cut>,
        }
        impl Read for EndingReader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if !self.sent {
                    self.sent = true;
                    bytes[..4].copy_from_slice(&[1, 0, 2, 0]);
                    return Ok(4);
                }
                let mut settings = Settings {
                    model: "gpt-6-astra".into(),
                    reasoning_effort: "high".into(),
                    ..Settings::default()
                };
                settings.actions[0].prompt = "at the click".into();
                self.cuts.send(None).unwrap();
                self.cuts
                    .send(Some(settings.answer_options("answer").unwrap()))
                    .unwrap();
                self.cuts
                    .send(Some(settings.answer_options("brief").unwrap()))
                    .unwrap();
                Ok(0)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let first = WaveFile::create(&directory.path().join("audio/000001.wav")).unwrap();
        let (cut_tx, cut_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        record(
            EndingReader {
                sent: false,
                cuts: cut_tx,
            },
            first,
            directory.path().into(),
            1,
            0,
            cut_rx,
            &events_tx,
            &AtomicU64::new(0),
        )
        .unwrap();
        let events: Vec<_> = events_rx.try_iter().collect();
        assert!(matches!(
            &events[0],
            CaptureEvent::Cut {
                seq: 1,
                end_sample: 2,
                request: None,
                continuing: false
            }
        ));
        let requests: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                CaptureEvent::Empty { request } => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            (&*requests[0].action.id, &*requests[1].action.id),
            ("answer", "brief")
        );
        assert_eq!(
            (
                &*requests[0].action.prompt,
                &*requests[0].model,
                &*requests[0].reasoning_effort
            ),
            ("at the click", "gpt-6-astra", "high")
        );
        assert!(!directory.path().join("audio/000002.wav").exists());
    }
    #[test]
    fn wave_checkpoints_and_crash_recovery_keep_every_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        let mut file = WaveFile::create(&path).unwrap();
        file.write(&[1, 0, 2, 0]).unwrap();
        file.checkpoint().unwrap();
        file.write(&[3, 0, 4, 0]).unwrap();
        drop(file);
        assert_eq!(recover_wave(&path).unwrap(), 4);
        assert_eq!(
            hound::WavReader::open(&path)
                .unwrap()
                .samples::<i16>()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
    }
    #[test]
    fn segmentation_preserves_pcm_even_when_reads_end_between_sample_bytes() {
        struct OddReader {
            data: Vec<u8>,
            at: usize,
            tx: Sender<Cut>,
        }
        impl Read for OddReader {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.at == self.data.len() {
                    return Ok(0);
                }
                let n = (self.data.len() - self.at).min(3);
                b[..n].copy_from_slice(&self.data[self.at..self.at + n]);
                self.at += n;
                if self.at == 3 {
                    self.tx
                        .send(Some(Settings::default().answer_options("brief").unwrap()))
                        .unwrap();
                    self.tx
                        .send(Some(Settings::default().answer_options("clarify").unwrap()))
                        .unwrap();
                }
                Ok(n)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let first = WaveFile::create(&dir.path().join("audio/000001.wav")).unwrap();
        let (tx, rx) = mpsc::channel();
        let (et, er) = mpsc::channel();
        let data: Vec<u8> = (0..20_i16).flat_map(i16::to_le_bytes).collect();
        record(
            OddReader {
                data: data.clone(),
                at: 0,
                tx,
            },
            first,
            dir.path().into(),
            1,
            0,
            rx,
            &et,
            &AtomicU64::new(0),
        )
        .unwrap();
        let mut joined = vec![];
        for name in ["audio/000001.wav", "audio/000002.wav"] {
            joined.extend(
                hound::WavReader::open(dir.path().join(name))
                    .unwrap()
                    .samples::<i16>()
                    .map(Result::unwrap),
            );
        }
        assert_eq!(joined, (0..20).collect::<Vec<i16>>());
        let requests: Vec<_> = er
            .try_iter()
            .filter_map(|event| match event {
                CaptureEvent::Cut { request, .. } => request,
                CaptureEvent::Empty { request } => Some(request),
                _ => None,
            })
            .map(|request| request.action.id)
            .collect();
        assert_eq!(requests, ["brief", "clarify"]);
    }
}
