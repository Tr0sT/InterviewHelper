use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

fn socket_path() -> Result<PathBuf, String> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or("Не установлен XDG_RUNTIME_DIR.")?;
    let meta = fs::metadata(&root).map_err(|e| e.to_string())?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
        return Err("Небезопасный каталог пользовательской сессии.".into());
    }
    Ok(root.join("interview-helper.sock"))
}

pub fn send(command: &str) -> Result<Value, String> {
    send_to(&socket_path()?, command)
}

fn send_to(path: &Path, command: &str) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{command}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&reply).map_err(|e| e.to_string())
}

pub struct Server {
    path: PathBuf,
    listener: UnixListener,
}
impl Server {
    pub fn bind() -> Result<Self, String> {
        Self::bind_at(socket_path()?)
    }
    fn bind_at(path: PathBuf) -> Result<Self, String> {
        if path.exists() {
            let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                return Err("Путь управляющего сокета занят другим файлом.".into());
            }
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        Ok(Self { path, listener })
    }
    pub fn start(&self, handler: impl Fn(&str) -> Value + Send + 'static) -> Result<(), String> {
        let listener = self.listener.try_clone().map_err(|e| e.to_string())?;
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut command = String::new();
                let read = BufReader::new((&stream).take(129)).read_line(&mut command);
                let reply = if read.is_ok() && command.len() < 128 {
                    handler(command.trim())
                } else {
                    json!({"error":"Некорректная команда."})
                };
                let _ = writeln!(stream, "{reply}");
            }
        });
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_keep_private_permissions_limits_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control.sock");
        let server = Server::bind_at(path.clone()).unwrap();
        server.start(|command| json!({"command":command})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(send_to(&path, "status").unwrap()["command"], "status");
        assert!(send_to(&path, &"x".repeat(128))
            .unwrap()
            .get("error")
            .is_some());
        drop(server);
        assert!(!path.exists());
    }
    #[test]
    fn existing_regular_files_are_not_removed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control.sock");
        fs::write(&path, "keep").unwrap();
        assert!(Server::bind_at(path.clone()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "keep");
    }
}
