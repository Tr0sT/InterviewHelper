use crate::{
    audio::{record_pcm, CaptureEvent, CutRequest, Device, Devices, WaveFile},
    session::{AnswerOptions, Settings, SAMPLE_RATE},
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
};
use std::{
    collections::VecDeque,
    io::{self, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

fn device_id(device: &cpal::Device) -> Result<String, String> {
    device
        .id()
        .map(|id| id.to_string())
        .map_err(|e| e.to_string())
}
fn device_info(device: cpal::Device) -> Result<Device, String> {
    Ok(Device {
        name: device_id(&device)?,
        description: device.to_string(),
    })
}
pub fn devices() -> Result<Devices, String> {
    let host = cpal::default_host();
    let microphones = host
        .input_devices()
        .map_err(|e| format!("Не удалось получить микрофоны: {e}"))?
        .map(device_info)
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = host
        .output_devices()
        .map_err(|e| format!("Не удалось получить выходы: {e}"))?
        .map(device_info)
        .collect::<Result<Vec<_>, _>>()?;
    let default_microphone = host
        .default_input_device()
        .map(|d| device_id(&d))
        .transpose()?
        .unwrap_or_default();
    let default_output = host
        .default_output_device()
        .map(|d| device_id(&d))
        .transpose()?
        .unwrap_or_default();
    Ok(Devices {
        microphones,
        outputs,
        default_microphone,
        default_output,
    })
}
fn find_device(id: &str, microphone: bool) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let devices = if microphone {
        host.input_devices()
    } else {
        host.output_devices()
    }
    .map_err(|e| e.to_string())?;
    devices
        .into_iter()
        .find(|device| device_id(device).is_ok_and(|value| value == id))
        .ok_or_else(|| {
            if microphone {
                "Микрофон недоступен. Выберите другое устройство."
            } else {
                "Выход системного звука недоступен. Выберите другое устройство."
            }
            .into()
        })
}

// Stateful box-filter downsampling. Typical WASAPI device rates are 44.1 or 48 kHz.
struct Resampler {
    rate: u32,
    channels: usize,
    phase: u32,
    sum: f32,
    count: u32,
}
impl Resampler {
    fn new(rate: u32, channels: u16) -> Result<Self, String> {
        if rate < SAMPLE_RATE || channels == 0 {
            return Err(format!(
                "Формат аудио {rate} Гц / {channels} каналов не поддерживается."
            ));
        }
        Ok(Self {
            rate,
            channels: channels as usize,
            phase: 0,
            sum: 0.0,
            count: 0,
        })
    }
    fn convert<T: Sample>(&mut self, input: &[T]) -> Vec<f32>
    where
        f32: FromSample<T>,
    {
        let mut output = Vec::with_capacity(
            input.len() / self.channels * SAMPLE_RATE as usize / self.rate as usize + 2,
        );
        for frame in input.chunks_exact(self.channels) {
            let sample =
                frame.iter().map(|v| f32::from_sample(*v)).sum::<f32>() / self.channels as f32;
            self.sum += if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            self.count += 1;
            self.phase += SAMPLE_RATE;
            if self.phase >= self.rate {
                output.push(self.sum / self.count as f32);
                self.phase -= self.rate;
                self.sum = 0.0;
                self.count = 0;
            }
        }
        output
    }
}
struct Packet {
    start: u64,
    samples: Vec<f32>,
    arrived: Instant,
}
enum Input {
    Mic(Packet),
    Output(Packet),
    Error(String),
}
fn build_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mic: bool,
    tx: SyncSender<Input>,
    overflow: Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut converter = Resampler::new(config.sample_rate, config.channels)?;
    let err_tx = tx.clone();
    let err_overflow = overflow.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], info| {
                let samples = converter.convert(data);
                if samples.is_empty() {
                    return;
                }
                // On WASAPI both streams use the system performance counter as their clock.
                let nanos = info
                    .timestamp()
                    .capture
                    .duration_since(cpal::StreamInstant::ZERO)
                    .as_nanos();
                let start = ((nanos * SAMPLE_RATE as u128) / 1_000_000_000) as u64;
                let packet = Packet {
                    start,
                    samples,
                    arrived: Instant::now(),
                };
                let input = if mic {
                    Input::Mic(packet)
                } else {
                    Input::Output(packet)
                };
                if let Err(TrySendError::Full(_)) = tx.try_send(input) {
                    overflow.store(true, Ordering::Release);
                }
            },
            move |error| {
                // WASAPI loopback signals a discontinuity when playback starts after silence.
                // Output is aligned by capture timestamp, so missing periods remain silence.
                if !mic && error.kind() == cpal::ErrorKind::Xrun {
                    return;
                }
                if let Err(TrySendError::Full(_)) = err_tx.try_send(Input::Error(format!(
                    "Ошибка {}: {error} ({:?})",
                    if mic {
                        "микрофона"
                    } else {
                        "системного выхода"
                    },
                    error.kind()
                ))) {
                    err_overflow.store(true, Ordering::Release);
                }
            },
            None,
        )
        .map_err(|e| format!("Не удалось открыть аудиопоток: {e}"))
}
fn open_stream(
    device: &cpal::Device,
    mic: bool,
    tx: SyncSender<Input>,
    overflow: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
    let supported = if mic {
        device.default_input_config()
    } else {
        device.default_output_config()
    }
    .map_err(|e| format!("Не удалось определить формат аудиоустройства: {e}"))?;
    let config = supported.config();
    match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(device, config, mic, tx, overflow),
        SampleFormat::F64 => build_stream::<f64>(device, config, mic, tx, overflow),
        SampleFormat::I16 => build_stream::<i16>(device, config, mic, tx, overflow),
        SampleFormat::I24 => build_stream::<cpal::I24>(device, config, mic, tx, overflow),
        SampleFormat::I32 => build_stream::<i32>(device, config, mic, tx, overflow),
        SampleFormat::U16 => build_stream::<u16>(device, config, mic, tx, overflow),
        SampleFormat::U32 => build_stream::<u32>(device, config, mic, tx, overflow),
        format => Err(format!("Формат аудио {format:?} пока не поддерживается.")),
    }
}
fn mix(mic: Packet, output: &mut VecDeque<Packet>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(mic.samples.len() * 2);
    for (i, sample) in mic.samples.into_iter().enumerate() {
        let index = mic.start + i as u64;
        while output
            .front()
            .is_some_and(|block| block.start + block.samples.len() as u64 <= index)
        {
            output.pop_front();
        }
        let system = output
            .front()
            .and_then(|block| {
                index
                    .checked_sub(block.start)
                    .and_then(|n| block.samples.get(n as usize))
            })
            .copied()
            .unwrap_or(0.0);
        let mixed = ((sample + system) * 0.5).clamp(-1.0, 1.0);
        bytes.extend_from_slice(&((mixed * i16::MAX as f32).round() as i16).to_le_bytes());
    }
    bytes
}
struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
    at: usize,
}
impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.at == self.bytes.len() {
            match self.rx.recv() {
                Ok(bytes) => {
                    self.bytes = bytes;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.bytes.len() - self.at);
        out[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}
pub struct Capture {
    cuts: Sender<CutRequest>,
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
        self.stopping.store(true, Ordering::Release);
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
    let microphone = find_device(&settings.microphone, true)?;
    let output = find_device(&settings.output, false)?;
    let first = WaveFile::create(&directory.join(format!("audio/{seq:06}.wav")))?;
    let (input_tx, input_rx) = mpsc::sync_channel::<Input>(128);
    let (pcm_tx, pcm_rx) = mpsc::sync_channel::<Vec<u8>>(64);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let stopping = Arc::new(AtomicBool::new(false));
    let overflow = Arc::new(AtomicBool::new(false));
    let stop_mixer = stopping.clone();
    let mixer_events = events.clone();
    thread::spawn(move || {
        let run = || -> Result<(cpal::Stream, cpal::Stream), String> {
            let output_stream = open_stream(&output, false, input_tx.clone(), overflow.clone())?;
            let mic_stream = open_stream(&microphone, true, input_tx, overflow.clone())?;
            output_stream
                .play()
                .map_err(|e| format!("Не удалось запустить системный звук: {e}"))?;
            mic_stream
                .play()
                .map_err(|e| format!("Не удалось запустить микрофон: {e}"))?;
            Ok((output_stream, mic_stream))
        };
        let (_output_stream, _mic_stream) = match run() {
            Ok(streams) => {
                let _ = ready_tx.send(Ok(()));
                streams
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let mut mic_queue: VecDeque<Packet> = VecDeque::new();
        let mut output_queue: VecDeque<Packet> = VecDeque::new();
        let mut last_mic = Instant::now();
        let mut failed = None;
        while !stop_mixer.load(Ordering::Acquire) {
            match input_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Input::Mic(block)) => {
                    last_mic = Instant::now();
                    mic_queue.push_back(block);
                }
                Ok(Input::Output(block)) => output_queue.push_back(block),
                Ok(Input::Error(e)) => {
                    failed = Some(e);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    failed = Some("Аудиопоток завершился.".into());
                    break;
                }
            }
            if overflow.load(Ordering::Acquire) {
                failed = Some("Аудиопоток не успевает обрабатываться; запись остановлена.".into());
                break;
            }
            if last_mic.elapsed() > Duration::from_secs(15) {
                failed = Some("Микрофон не выдаёт данные более 15 секунд.".into());
                break;
            }
            // A short buffer aligns the two independent WASAPI callback threads by capture timestamp.
            while mic_queue
                .front()
                .is_some_and(|p| p.arrived.elapsed() >= Duration::from_millis(120))
            {
                let bytes = mix(mic_queue.pop_front().unwrap(), &mut output_queue);
                if pcm_tx.try_send(bytes).is_err() {
                    failed = Some("Не удалось вовремя сохранить звук.".into());
                    break;
                }
            }
            if failed.is_some() {
                break;
            }
            while output_queue
                .front()
                .is_some_and(|p| p.arrived.elapsed() > Duration::from_secs(2))
            {
                output_queue.pop_front();
            }
        }
        // Stop callbacks, then preserve all microphone packets already received and drain pending input.
        drop(_mic_stream);
        drop(_output_stream);
        while let Ok(item) = input_rx.try_recv() {
            match item {
                Input::Mic(p) => mic_queue.push_back(p),
                Input::Output(p) => output_queue.push_back(p),
                Input::Error(e) => {
                    if failed.is_none() {
                        failed = Some(e);
                    }
                }
            }
        }
        for block in mic_queue {
            if pcm_tx.send(mix(block, &mut output_queue)).is_err() {
                break;
            }
        }
        if let Some(error) = failed {
            let _ = mixer_events.send(CaptureEvent::Failed(error));
        }
        stop_mixer.store(true, Ordering::Release);
        // Dropping pcm_tx finishes record_pcm, which sends Stopped only after checkpointing.
    });
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            drop(first);
            let _ = std::fs::remove_file(directory.join(format!("audio/{seq:06}.wav")));
            return Err(error);
        }
        Err(error) => {
            drop(first);
            let _ = std::fs::remove_file(directory.join(format!("audio/{seq:06}.wav")));
            return Err(error.to_string());
        }
    }
    let (cut_tx, cut_rx) = mpsc::channel();
    let stop_reader = stopping.clone();
    thread::spawn(move || {
        let reader = ChannelReader {
            rx: pcm_rx,
            bytes: vec![],
            at: 0,
        };
        let result = record_pcm(
            reader,
            first,
            directory,
            seq,
            start_sample,
            cut_rx,
            &events,
            &AtomicU64::new(0),
        );
        if let Err(error) = result {
            let _ = events.send(CaptureEvent::Failed(error));
            stop_reader.store(true, Ordering::Release);
        }
        let _ = events.send(CaptureEvent::Stopped);
    });
    Ok(Capture {
        cuts: cut_tx,
        stopping,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampling_preserves_duration_across_callback_boundaries() {
        let mut converter = Resampler::new(48_000, 2).unwrap();
        let a = converter.convert(&vec![0.5f32; 482]);
        let b = converter.convert(&vec![0.5f32; 478]);
        assert_eq!(a.len() + b.len(), 240);
        let mut converter = Resampler::new(44_100, 1).unwrap();
        let mut count = 0;
        for chunk in vec![0.25f32; 44_100].chunks(137) {
            count += converter.convert(chunk).len();
        }
        assert_eq!(count, 24_000);
    }
    #[test]
    fn mixer_uses_timestamps_and_fills_silent_output() {
        let packet = |start, samples: Vec<f32>| Packet {
            start,
            samples,
            arrived: Instant::now(),
        };
        let mut system = VecDeque::from([packet(101, vec![0.6, 0.4])]);
        let bytes = mix(packet(100, vec![0.2, 0.2, 0.2, 0.2]), &mut system);
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| i16::from_le_bytes(*s))
            .collect();
        assert_eq!(samples, vec![3277, 13107, 9830, 3277]);
    }
}
