use crate::{
    audio::{record_pcm, CaptureEvent, CutRequest, Device, Devices, WaveFile},
    session::{AnswerOptions, Settings},
};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

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

pub struct Capture {
    cuts: Sender<CutRequest>,
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
        let result = record_pcm(
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
}
