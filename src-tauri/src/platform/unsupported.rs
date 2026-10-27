fn unavailable(feature: &str) -> String {
    format!("{feature} пока не реализован для {}.", std::env::consts::OS)
}

pub mod capture {
    use crate::{
        audio::{CaptureEvent, Devices},
        session::{AnswerOptions, Settings},
    };
    use std::{path::PathBuf, sync::mpsc::Sender};
    pub struct Capture;
    impl Capture {
        pub fn cut(&self, _: Option<AnswerOptions>) -> Result<(), String> {
            Err(super::unavailable("Захват звука"))
        }
        pub fn stop(&self) -> Result<(), String> {
            Err(super::unavailable("Захват звука"))
        }
    }
    pub fn devices() -> Result<Devices, String> {
        Err(super::unavailable("Захват звука"))
    }
    pub fn start(
        _: PathBuf,
        _: Settings,
        _: u64,
        _: u64,
        _: Sender<CaptureEvent>,
    ) -> Result<Capture, String> {
        Err(super::unavailable("Захват звука"))
    }
}

pub mod hotkeys {
    use crate::session::Settings;
    use std::path::PathBuf;
    pub struct Hotkeys;
    impl Hotkeys {
        pub fn new(_: PathBuf) -> Self {
            Self
        }
        pub fn replace(&self, _: &Settings) -> Result<(), String> {
            Err(super::unavailable("Адаптер горячих клавиш"))
        }
        pub fn clear(&self) {}
    }
}

pub mod control {
    use serde_json::Value;
    pub fn send(_: &str) -> Result<Value, String> {
        Err(super::unavailable("Локальный канал управления"))
    }
    pub struct Server;
    impl Server {
        pub fn close(&self) {}
        pub fn bind() -> Result<Self, String> {
            Err(super::unavailable("Локальный канал управления"))
        }
        pub fn start(&self, _: impl Fn(&str) -> Value + Send + 'static) -> Result<(), String> {
            Err(super::unavailable("Локальный канал управления"))
        }
    }
}
