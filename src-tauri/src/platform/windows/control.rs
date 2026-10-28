use interprocess::{
    local_socket::{
        prelude::*, GenericNamespaced, Listener, ListenerNonblockingMode, ListenerOptions, Stream,
    },
    os::windows::{local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor},
};
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    os::windows::process::CommandExt,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use widestring::U16CString;

// The pipe is local-only and restricted to the current account. A session identifier
// avoids one interactive Windows session controlling another instance of the app.
fn identity() -> Result<&'static (String, String), String> {
    static IDENTITY: OnceLock<Result<(String, String), String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let exe = std::env::var_os("SystemRoot")
                .map(std::path::PathBuf::from)
                .ok_or("Не найден каталог Windows.")?
                .join("System32")
                .join("whoami.exe");
            let output = Command::new(exe)
                .args(["/user", "/fo", "csv", "/nh"])
                .creation_flags(0x0800_0000)
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err("Не удалось определить пользователя Windows.".into());
            }
            let output = String::from_utf8_lossy(&output.stdout);
            let sid = output
                .split('"')
                .find(|part| {
                    part.starts_with("S-1-")
                        && part
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == 'S' || c == '-')
                })
                .ok_or("Не удалось определить SID пользователя Windows.")?
                .to_string();
            let mut session = 0u32;
            if unsafe {
                windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(
                    std::process::id(),
                    &mut session,
                )
            } == 0
            {
                return Err(io::Error::last_os_error().to_string());
            }
            Ok((format!("interview-helper-{sid}-{session}"), sid))
        })
        .as_ref()
        .map_err(Clone::clone)
}
fn pipe_name() -> Result<String, String> {
    Ok(identity()?.0.clone())
}
fn descriptor() -> Result<SecurityDescriptor, String> {
    let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", identity()?.1);
    let text = U16CString::from_str(sddl).map_err(|e| e.to_string())?;
    SecurityDescriptor::deserialize(&text).map_err(|e| e.to_string())
}

fn read_line(stream: &mut Stream, limit: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut until = Instant::now() + timeout;
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < until {
        match stream.read(&mut buf) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(n) => {
                until = Instant::now() + timeout;
                for &byte in &buf[..n] {
                    if byte == b'\n' {
                        return Ok(line);
                    }
                    line.push(byte);
                    if line.len() >= limit {
                        return Err("Слишком длинная команда или ответ.".into());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Истекло время ожидания управляющего канала.".into())
}
fn write_all(stream: &mut Stream, mut bytes: &[u8]) -> Result<(), String> {
    let mut until = Instant::now() + Duration::from_secs(5);
    while !bytes.is_empty() && Instant::now() < until {
        match stream.write(&bytes[..bytes.len().min(256)]) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(n) => {
                until = Instant::now() + Duration::from_secs(5);
                bytes = &bytes[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    if bytes.is_empty() {
        Ok(())
    } else {
        Err("Истекло время ожидания управляющего канала.".into())
    }
}
pub fn send(command: &str) -> Result<Value, String> {
    send_to(&pipe_name()?, command)
}
fn send_to(name: &str, command: &str) -> Result<Value, String> {
    if command.contains(['\n', '\r']) || command.len() >= 128 {
        return Err("Некорректная команда.".into());
    }
    let name = name
        .to_ns_name::<GenericNamespaced>()
        .map_err(|e| e.to_string())?;
    let mut stream = Stream::connect(name).map_err(|e| e.to_string())?;
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    write_all(&mut stream, format!("{command}\n").as_bytes())?;
    let line = read_line(&mut stream, 16_777_216, Duration::from_secs(5))?;
    serde_json::from_slice(&line).map_err(|e| e.to_string())
}
pub struct Server {
    listener: Mutex<Option<Listener>>,
    closed: Arc<AtomicBool>,
}
impl Server {
    pub fn bind() -> Result<Self, String> {
        Self::bind_at(&pipe_name()?)
    }
    fn bind_at(name: &str) -> Result<Self, String> {
        let name = name
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| e.to_string())?;
        let listener = ListenerOptions::new()
            .name(name)
            .security_descriptor(descriptor()?)
            .nonblocking(ListenerNonblockingMode::Both)
            .create_sync()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            listener: Mutex::new(Some(listener)),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn start(&self, handler: impl Fn(&str) -> Value + Send + 'static) -> Result<(), String> {
        let listener = self
            .listener
            .lock()
            .map_err(|e| e.to_string())?
            .take()
            .ok_or("Канал уже запущен.")?;
        let closed = self.closed.clone();
        thread::spawn(move || {
            while !closed.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(mut stream) => {
                        let reply = match read_line(&mut stream, 128, Duration::from_secs(2)) {
                            Ok(line) if !line.is_empty() => match std::str::from_utf8(&line) {
                                Ok(command) => handler(command.trim()),
                                Err(_) => json!({"error":"Некорректная команда."}),
                            },
                            Ok(_) => continue,
                            Err(_) => json!({"error":"Некорректная команда."}),
                        };
                        let _ = write_all(&mut stream, format!("{reply}\n").as_bytes());
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(())
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_pipe_roundtrips_and_releases_name() {
        let name = format!("{}-test-{}", pipe_name().unwrap(), std::process::id());
        let server = Server::bind_at(&name).unwrap();
        server
            .start(|command| {
                if command == "big" {
                    json!({"text":"ю".repeat(5000)})
                } else {
                    json!({"command":command})
                }
            })
            .unwrap();
        assert_eq!(send_to(&name, "status").unwrap()["command"], "status");
        assert_eq!(
            send_to(&name, "big").unwrap()["text"]
                .as_str()
                .unwrap()
                .len(),
            10000
        );
        assert!(send_to(&name, &"x".repeat(128)).is_err());
        server.close();
        let mut released = false;
        for _ in 0..100 {
            if Server::bind_at(&name).is_ok() {
                released = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(released, "pipe name should be released after shutdown");
    }
}
