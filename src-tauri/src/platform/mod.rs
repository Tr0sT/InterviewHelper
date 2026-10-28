pub mod files;

#[cfg(target_os = "linux")]
#[path = "linux/capture.rs"]
pub mod capture;
#[cfg(target_os = "linux")]
#[path = "linux/control.rs"]
pub mod control;
#[cfg(target_os = "linux")]
#[path = "linux/hotkeys.rs"]
pub mod hotkeys;

#[cfg(target_os = "windows")]
#[path = "windows/capture.rs"]
pub mod capture;
#[cfg(target_os = "windows")]
#[path = "windows/control.rs"]
pub mod control;
#[cfg(target_os = "windows")]
#[path = "windows/hotkeys.rs"]
pub mod hotkeys;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use unsupported::{capture, control, hotkeys};

use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn data_dir() -> Result<PathBuf, String> {
    std::env::var_os("INTERVIEW_HELPER_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|root| root.join("interview-helper")))
        .ok_or_else(|| "Не найден каталог данных пользователя.".into())
}

pub fn codex_auth_path() -> Result<PathBuf, String> {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or("Не найден каталог Codex.")?;
    Ok(root.join("auth.json"))
}

pub fn transcription_user_agent() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    format!("Codex Desktop/1.0.4 ({}; {arch})", std::env::consts::OS)
}

pub fn open_path(path: &Path) -> Result<(), String> {
    let program = if cfg!(target_os = "linux") {
        "xdg-open"
    } else if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer.exe"
    } else {
        return Err("Открытие файлов пока не поддерживается на этой ОС.".into());
    };
    let mut child = Command::new(program)
        .arg(path)
        .spawn()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
