use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("Не удалось создать {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn private_open_options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.mode(0o600);
        options
    }
    #[cfg(not(unix))]
    {
        // Windows inherits the containing user directory's ACL.
        OpenOptions::new()
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Путь файла без каталога.")?;
    private_dir(parent)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = private_open_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)
        .map_err(|e| format!("Ошибка сохранения: {e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Ошибка записи: {e}"))?;
    drop(file);
    fs::rename(&temp, path).map_err(|e| format!("Не удалось обновить файл: {e}"))?;
    #[cfg(unix)]
    {
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("Не удалось синхронизировать каталог: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_write_replaces_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("session/context.txt");
        atomic_write(&file, b"first").unwrap();
        atomic_write(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn unix_data_permissions_remain_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("session/context.txt");
        atomic_write(&file, b"private").unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
