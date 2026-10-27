#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    Control,
    Alt,
    Shift,
    Super,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub modifiers: Vec<Modifier>,
    pub key: String,
}
pub fn parse(text: &str) -> Result<Shortcut, String> {
    let parts: Vec<_> = text.split('+').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err("Некорректная горячая клавиша.".into());
    }
    let mut modifiers = vec![];
    for part in &parts[..parts.len() - 1] {
        let modifier = match part.to_lowercase().as_str() {
            "ctrl" => Modifier::Control,
            "alt" => Modifier::Alt,
            "shift" => Modifier::Shift,
            "super" => Modifier::Super,
            _ => return Err(format!("Неизвестный модификатор: {part}")),
        };
        if modifiers.contains(&modifier) {
            return Err("Модификатор указан дважды.".into());
        }
        modifiers.push(modifier);
    }
    modifiers.sort_unstable();
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
    Ok(Shortcut { modifiers, key })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcuts_cannot_inject_compositor_or_shell_commands() {
        assert_eq!(
            parse("Ctrl+Alt+Enter").unwrap(),
            parse("Alt+Ctrl+Return").unwrap()
        );
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
