use crate::session::Settings;
use serde_json::Value;
use std::{path::PathBuf, process::Command, sync::Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub modifiers: String,
    pub mask: u64,
    pub key: String,
}
pub fn parse(text: &str) -> Result<Shortcut, String> {
    let parts: Vec<_> = text.split('+').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err("Некорректная горячая клавиша.".into());
    }
    let mut modifiers = vec![];
    let mut mask = 0;
    for part in &parts[..parts.len() - 1] {
        let (name, bit) = match part.to_lowercase().as_str() {
            "ctrl" => ("CTRL", 4),
            "alt" => ("ALT", 8),
            "shift" => ("SHIFT", 1),
            "super" => ("SUPER", 64),
            _ => return Err(format!("Неизвестный модификатор: {part}")),
        };
        if mask & bit != 0 {
            return Err("Модификатор указан дважды.".into());
        }
        mask |= bit;
        modifiers.push(name);
    }
    let raw = parts.last().expect("nonempty").to_uppercase();
    let key = match raw.as_str() {
        "ENTER" | "RETURN" => "RETURN".into(),
        "SPACE" => "SPACE".into(),
        _ if raw.len() == 1 && raw.as_bytes()[0].is_ascii_alphanumeric() => raw,
        _ if raw.starts_with('F')
            && raw[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            raw
        }
        _ => return Err("Используйте букву, цифру, Space, Enter или F1–F24.".into()),
    };
    Ok(Shortcut {
        modifiers: modifiers.join(" "),
        mask,
        key,
    })
}
#[derive(Clone)]
struct Binding {
    shortcut: Shortcut,
    argument: String,
    description: String,
}
impl Shortcut {
    fn lua_keys(&self) -> String {
        self.modifiers
            .split_whitespace()
            .chain(std::iter::once(self.key.as_str()))
            .collect::<Vec<_>>()
            .join(" + ")
    }
}
pub struct Hotkeys {
    executable: PathBuf,
    installed: Mutex<Vec<Binding>>,
}
impl Hotkeys {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            installed: Mutex::new(vec![]),
        }
    }
    fn bindings() -> Result<Vec<Value>, String> {
        let result = Command::new("hyprctl")
            .args(["-j", "binds"])
            .output()
            .map_err(|e| format!("Hyprland недоступен: {e}"))?;
        if !result.status.success() {
            return Err("Глобальные клавиши этой версии требуют работающий Hyprland.".into());
        }
        serde_json::from_slice(&result.stdout).map_err(|e| e.to_string())
    }
    fn matches(value: &Value, shortcut: &Shortcut) -> bool {
        value["modmask"].as_u64() == Some(shortcut.mask)
            && value["key"]
                .as_str()
                .is_some_and(|key| key.eq_ignore_ascii_case(&shortcut.key))
    }
    fn eval(code: String) -> Result<(), String> {
        let result = Command::new("hyprctl")
            .args(["eval", &code])
            .output()
            .map_err(|e| e.to_string())?;
        let response = String::from_utf8_lossy(&result.stdout);
        if !result.status.success() || response.trim() != "ok" {
            return Err(format!(
                "Не удалось зарегистрировать клавишу: {}",
                response.trim()
            ));
        }
        Ok(())
    }
    fn install(binding: &Binding) -> Result<(), String> {
        Self::eval(format!(
            "hl.bind({}, hl.dsp.exec_cmd({}), {{description={}}})",
            serde_json::to_string(&binding.shortcut.lua_keys()).map_err(|e| e.to_string())?,
            serde_json::to_string(&binding.argument).map_err(|e| e.to_string())?,
            serde_json::to_string(&binding.description).map_err(|e| e.to_string())?
        ))
    }
    fn remove_owned(binding: &Binding) -> Result<(), String> {
        let list = Self::bindings()?;
        let matches: Vec<_> = list
            .iter()
            .filter(|value| Self::matches(value, &binding.shortcut))
            .collect();
        if matches.is_empty() {
            return Ok(());
        }
        if matches.len() != 1 || matches[0]["description"] != binding.description {
            return Err("Сочетание было изменено вне приложения; чужая привязка сохранена.".into());
        }
        Self::eval(format!(
            "hl.unbind({})",
            serde_json::to_string(&binding.shortcut.lua_keys()).map_err(|e| e.to_string())?
        ))
    }
    pub fn replace(&self, settings: &Settings) -> Result<(), String> {
        settings.validate()?;
        let executable = self.executable.to_string_lossy();
        if executable.contains([',', '\n', '\r']) {
            return Err("Путь приложения несовместим с привязками Hyprland.".into());
        }
        let escaped = format!("'{}'", executable.replace('\'', "'\\''"));
        let mut desired = vec![Binding {
            shortcut: parse(&settings.cut_shortcut)?,
            argument: format!("{escaped} --command cut"),
            description: format!("Interview Helper:{executable}:cut"),
        }];
        for action in &settings.actions {
            if !action.shortcut.trim().is_empty() {
                desired.push(Binding {
                    shortcut: parse(&action.shortcut)?,
                    argument: format!("{escaped} --command action:{}", action.id),
                    description: format!("Interview Helper:{executable}:action:{}", action.id),
                });
            }
        }
        let mut installed = self.installed.lock().map_err(|e| e.to_string())?;
        let existing = Self::bindings()?;
        if installed.is_empty() {
            for value in &existing {
                for wanted in &desired {
                    if value["description"] == wanted.description
                        && Self::matches(value, &wanted.shortcut)
                    {
                        installed.push(wanted.clone());
                    }
                }
            }
        }
        for wanted in &desired {
            for value in existing
                .iter()
                .filter(|value| Self::matches(value, &wanted.shortcut))
            {
                if !installed.iter().any(|b| {
                    Self::matches(value, &b.shortcut) && value["description"] == b.description
                }) {
                    return Err(format!(
                        "Сочетание {}+{} занято. Выберите другое в настройках.",
                        wanted.shortcut.modifiers, wanted.shortcut.key
                    ));
                }
            }
        }
        let previous = installed.clone();
        for old in &previous {
            Self::remove_owned(old)?;
        }
        installed.clear();
        for wanted in desired {
            if let Err(error) = Self::install(&wanted) {
                for bound in installed.iter() {
                    let _ = Self::remove_owned(bound);
                }
                installed.clear();
                for old in previous {
                    if Self::install(&old).is_ok() {
                        installed.push(old);
                    }
                }
                return Err(error);
            }
            installed.push(wanted);
        }
        Ok(())
    }
    pub fn clear(&self) {
        if let Ok(mut installed) = self.installed.lock() {
            for binding in installed.iter() {
                let _ = Self::remove_owned(binding);
            }
            installed.clear();
        }
    }
}
impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcuts_cannot_inject_compositor_or_shell_commands() {
        assert_eq!(parse("Ctrl+Alt+Enter").unwrap().mask, 12);
        assert_eq!(parse("Space").unwrap().key, "SPACE");
        for bad in [
            "Ctrl+Alt+Space,exec,evil",
            "Ctrl+$(id)",
            "Ctrl+Ctrl+K",
            "Ctrl+",
            "F99",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
