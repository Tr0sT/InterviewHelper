#![cfg(target_os = "windows")]
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, SampleFormat, SizedSample,
};
use interview_helper::{audio::CaptureEvent, platform::capture, session::Settings};
use std::{
    f32::consts::TAU,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn tone<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
) -> cpal::Stream {
    let channels = config.channels as usize;
    let rate = config.sample_rate as f32;
    let mut frame = 0u64;
    device
        .build_output_stream(
            config,
            move |buffer: &mut [T], _| {
                for samples in buffer.chunks_mut(channels) {
                    let value = ((frame as f32 * 440.0 * TAU / rate).sin()) * 0.04;
                    for sample in samples {
                        *sample = T::from_sample(value);
                    }
                    frame += 1;
                }
            },
            |e| eprintln!("Playback error: {e}"),
            None,
        )
        .expect("test playback stream")
}
#[test]
#[ignore = "uses real audio devices and plays a short, quiet test tone"]
fn real_windows_capture_contains_the_system_test_tone_and_checkpoints_segments() {
    let devices = capture::devices().expect("list audio devices");
    assert!(
        !devices.default_microphone.is_empty(),
        "no default microphone"
    );
    assert!(!devices.default_output.is_empty(), "no default output");
    let directory = tempfile::tempdir().unwrap();
    let settings = Settings {
        microphone: devices.default_microphone,
        output: devices.default_output,
        ..Settings::default()
    };
    let (tx, rx) = mpsc::channel();
    let capture =
        capture::start(directory.path().into(), settings, 1, 0, tx).expect("start capture");
    let output = cpal::default_host()
        .default_output_device()
        .expect("default output");
    let supported = output.default_output_config().unwrap();
    let config = supported.config();
    let player = match supported.sample_format() {
        SampleFormat::F32 => tone::<f32>(&output, config),
        SampleFormat::I16 => tone::<i16>(&output, config),
        SampleFormat::I32 => tone::<i32>(&output, config),
        SampleFormat::U16 => tone::<u16>(&output, config),
        format => panic!("unsupported test output format: {format:?}"),
    };
    player.play().unwrap();
    thread::sleep(Duration::from_millis(1200));
    if let Err(error) = capture.cut(None) {
        let mut details = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                CaptureEvent::Failed(message) => details.push(message),
                CaptureEvent::Stopped => details.push("stopped".into()),
                _ => {}
            }
        }
        panic!("cut failed: {error}; events: {details:?}");
    }
    thread::sleep(Duration::from_millis(1300));
    capture.stop().unwrap();
    drop(player);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cuts = 0;
    let mut stopped = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Failed(error)) => panic!("capture failed: {error}"),
            Ok(CaptureEvent::Cut { .. }) => cuts += 1,
            Ok(CaptureEvent::Stopped) => {
                stopped = true;
                break;
            }
            _ => {}
        }
    }
    assert!(stopped, "capture did not stop");
    assert_eq!(cuts, 2, "cut and stop should finalize two segments");
    let mut all = Vec::new();
    for name in ["audio/000001.wav", "audio/000002.wav"] {
        let mut wav = hound::WavReader::open(directory.path().join(name)).unwrap();
        assert_eq!(wav.spec().channels, 1);
        assert_eq!(wav.spec().sample_rate, 24_000);
        all.extend(wav.samples::<i16>().map(Result::unwrap));
    }
    assert!(
        all.len() > 24_000 * 2,
        "audio is unexpectedly short: {} samples",
        all.len()
    );
    let mut sin = 0.0f64;
    let mut cos = 0.0f64;
    for (i, sample) in all.iter().enumerate() {
        let phase = i as f64 * 440.0 * std::f64::consts::TAU / 24_000.0;
        sin += *sample as f64 * phase.sin();
        cos += *sample as f64 * phase.cos();
    }
    let amplitude = 2.0 * sin.hypot(cos) / all.len() as f64;
    println!(
        "captured {} samples; 440 Hz amplitude {:.1} / 32767",
        all.len(),
        amplitude
    );
    assert!(
        amplitude > 40.0,
        "the system test tone is missing from the mixed capture"
    );
}
