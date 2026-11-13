use crate::{
    hotkeys::{parse, Modifier, Shortcut},
    session::Settings,
};
use std::{
    path::PathBuf,
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    thread,
};
use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
            MOD_WIN,
        },
        WindowsAndMessaging::{
            GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY,
        },
    },
};

const UPDATE: u32 = WM_APP + 17;
#[derive(Clone)]
struct Binding {
    id: i32,
    shortcut: Shortcut,
    command: String,
}
enum Request {
    Replace(Vec<(Shortcut, String)>, Sender<Result<(), String>>),
    Shutdown,
}
fn key_code(shortcut: &Shortcut) -> Result<u32, String> {
    match shortcut.key.as_str() {
        "RETURN" => Ok(0x0d),
        "SPACE" => Ok(0x20),
        s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => Ok(s.as_bytes()[0] as u32),
        s if s.starts_with('F') => {
            let n: u32 = s[1..].parse().map_err(|_| "Некорректная клавиша.")?;
            if n == 12 {
                return Err("F12 зарезервирована Windows для отладки.".into());
            }
            if !(1..=24).contains(&n) {
                return Err("Некорректная клавиша.".into());
            }
            Ok(0x70 + n - 1)
        }
        _ => Err("Клавиша не поддерживается Windows.".into()),
    }
}
fn register(binding: &Binding) -> Result<(), String> {
    let mods = binding
        .shortcut
        .modifiers
        .iter()
        .fold(MOD_NOREPEAT, |mask, m| {
            mask | match m {
                Modifier::Control => MOD_CONTROL,
                Modifier::Alt => MOD_ALT,
                Modifier::Shift => MOD_SHIFT,
                Modifier::Super => MOD_WIN,
            }
        });
    let key = key_code(&binding.shortcut)?;
    if unsafe { RegisterHotKey(std::ptr::null_mut(), binding.id, mods, key) } == 0 {
        return Err(format!(
            "Сочетание {:?}+{} занято или недоступно в Windows.",
            binding.shortcut.modifiers, binding.shortcut.key
        ));
    }
    Ok(())
}
fn replace(
    installed: &mut Vec<Binding>,
    desired: Vec<(Shortcut, String)>,
    next_id: &mut i32,
) -> Result<(), String> {
    let mut staged = Vec::new();
    let mut next = Vec::new();
    for (shortcut, command) in desired {
        if let Some(old) = installed.iter().find(|old| old.shortcut == shortcut) {
            next.push(Binding {
                id: old.id,
                shortcut,
                command,
            });
        } else {
            if *next_id > 0xbfff {
                *next_id = 1;
            }
            while installed
                .iter()
                .chain(staged.iter())
                .any(|x: &Binding| x.id == *next_id)
            {
                *next_id += 1;
            }
            let binding = Binding {
                id: *next_id,
                shortcut,
                command,
            };
            *next_id += 1;
            if let Err(error) = register(&binding) {
                for b in &staged {
                    unsafe {
                        UnregisterHotKey(std::ptr::null_mut(), b.id);
                    }
                }
                return Err(error);
            }
            staged.push(binding.clone());
            next.push(binding);
        }
    }
    for old in installed
        .iter()
        .filter(|old| !next.iter().any(|new| new.id == old.id))
    {
        unsafe {
            UnregisterHotKey(std::ptr::null_mut(), old.id);
        }
    }
    *installed = next;
    Ok(())
}
pub struct Hotkeys {
    thread_id: u32,
    requests: Sender<Request>,
    gate: Mutex<()>,
}
impl Hotkeys {
    pub fn new(_: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut msg = MSG::default();
            // Ensure this thread owns a message queue before it is exposed to callers.
            unsafe {
                PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
            }
            let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
            let mut installed: Vec<Binding> = Vec::new();
            let mut next_id = 1;
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                if msg.message == UPDATE {
                    while let Ok(req) = rx.try_recv() {
                        match req {
                            Request::Replace(desired, reply) => {
                                let _ = reply.send(replace(&mut installed, desired, &mut next_id));
                            }
                            Request::Shutdown => {
                                let _ = replace(&mut installed, vec![], &mut next_id);
                                return;
                            }
                        }
                    }
                } else if msg.message == WM_HOTKEY {
                    if let Some(binding) = installed.iter().find(|b| b.id == msg.wParam as i32) {
                        let _ = crate::platform::control::send(&binding.command);
                    }
                }
            }
            let _ = replace(&mut installed, vec![], &mut next_id);
        });
        Self {
            thread_id: ready_rx.recv().unwrap_or(0),
            requests: tx,
            gate: Mutex::new(()),
        }
    }
    pub fn replace(&self, settings: &Settings) -> Result<(), String> {
        settings.validate()?;
        let mut desired = vec![(parse(&settings.cut_shortcut)?, "cut".to_string())];
        for action in &settings.actions {
            if !action.shortcut.trim().is_empty() {
                desired.push((parse(&action.shortcut)?, format!("action:{}", action.id)));
            }
        }
        self.update(desired)
    }
    fn update(&self, desired: Vec<(Shortcut, String)>) -> Result<(), String> {
        let _gate = self.gate.lock().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(Request::Replace(desired, tx))
            .map_err(|e| e.to_string())?;
        if unsafe { PostThreadMessageW(self.thread_id, UPDATE, 0, 0) } == 0 {
            return Err("Поток горячих клавиш недоступен.".into());
        }
        rx.recv().map_err(|e| e.to_string())?
    }
    pub fn clear(&self) {
        let _ = self.update(vec![]);
    }
}
impl Drop for Hotkeys {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        unsafe {
            PostThreadMessageW(self.thread_id, UPDATE, 0, 0);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_supported_keys_and_rejects_reserved_f12() {
        assert_eq!(key_code(&parse("Ctrl+Alt+Enter").unwrap()).unwrap(), 0x0d);
        assert_eq!(key_code(&parse("Ctrl+Alt+Space").unwrap()).unwrap(), 0x20);
        assert_eq!(key_code(&parse("Ctrl+F24").unwrap()).unwrap(), 0x87);
        assert!(key_code(&parse("Ctrl+F12").unwrap()).is_err());
    }
    #[test]
    #[ignore = "registers temporary global Windows hotkeys"]
    fn failed_replacement_keeps_old_registration_and_releases_new_one() {
        let hotkeys = Hotkeys::new(PathBuf::new());
        let mut settings = Settings::default();
        settings.actions.clear();
        settings.cut_shortcut = "Ctrl+Alt+Shift+F23".into();
        hotkeys.replace(&settings).unwrap();
        let mut changed = settings.clone();
        changed.cut_shortcut = "Ctrl+Alt+Shift+F24".into();
        let mut action = Settings::default().actions.remove(0);
        action.shortcut = "Ctrl+Alt+Shift+F12".into();
        changed.actions.push(action);
        assert!(hotkeys.replace(&changed).is_err());
        let mods = MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT;
        assert_eq!(
            unsafe { RegisterHotKey(std::ptr::null_mut(), 193, mods, 0x86) },
            0,
            "old F23 should still be registered"
        );
        assert_ne!(
            unsafe { RegisterHotKey(std::ptr::null_mut(), 194, mods, 0x87) },
            0,
            "staged F24 should have been released"
        );
        unsafe {
            UnregisterHotKey(std::ptr::null_mut(), 194);
        }
        hotkeys.clear();
        assert_ne!(
            unsafe { RegisterHotKey(std::ptr::null_mut(), 195, mods, 0x86) },
            0,
            "clear should release F23"
        );
        unsafe {
            UnregisterHotKey(std::ptr::null_mut(), 195);
        }
    }
}
