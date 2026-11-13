use std::{
    io,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub(crate) fn spawn_codex(args: &[&str]) -> io::Result<Child> {
    spawn_with_fallback(Path::new("codex.exe"), Path::new("codex.cmd"), args)
}

fn spawn_with_fallback(executable: &Path, npm_shim: &Path, args: &[&str]) -> io::Result<Child> {
    let spawn = |program: &Path| {
        Command::new(program)
            .args(args)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    match spawn(executable) {
        // Rust only adds .exe when resolving a program on Windows. npm installs
        // a .cmd shim instead; specifying its extension also lets Rust handle
        // batch-file argument escaping, including the quoted Codex -c values.
        Err(error) if error.kind() == io::ErrorKind::NotFound => spawn(npm_shim),
        result => result,
    }
}

pub(crate) fn stop_codex(child: &mut Child) {
    // The npm shim starts node, which starts the native Codex process. Killing
    // only our immediate child leaves app-server running after an RPC timeout.
    // Keep the child's process handle alive until the whole tree is stopped.
    let taskkill = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("taskkill.exe"))
        .unwrap_or_else(|| PathBuf::from("taskkill.exe"));
    let _ = Command::new(taskkill)
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    // Also reap the direct child, including when taskkill reports it already
    // exited or could not run.
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::mpsc,
        thread,
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };

    const HELPER: &str = "platform::process::tests::cmd_child";

    fn npm_shim(root: &Path) -> PathBuf {
        let shim = root.join("codex.cmd");
        // Match npm's %* forwarding and exercise a shim path containing spaces.
        std::fs::write(
            &shim,
            format!(
                "@ECHO off\r\n\"{}\" --exact {HELPER} --ignored --nocapture -- %*\r\n",
                std::env::current_exe().unwrap().display()
            ),
        )
        .unwrap();
        shim
    }

    #[test]
    fn npm_shim_preserves_quoted_config_arguments() {
        let root = tempfile::Builder::new()
            .prefix("codex npm test ")
            .tempdir()
            .unwrap();
        let args = [
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "model_reasoning_effort=\"low\"",
            "a path with spaces",
        ];
        let output = spawn_with_fallback(
            &root.path().join("codex.exe"),
            &npm_shim(root.path()),
            &args,
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let payload = stdout
            .lines()
            .find_map(|line| line.strip_prefix("CODEX_ARGS:"))
            .expect("the shim should launch the argument echo helper");
        let actual: Vec<String> = serde_json::from_str(payload).unwrap();
        assert_eq!(actual, args);
    }

    #[test]
    fn native_executable_is_preferred_and_invalid_executables_do_not_fall_back() {
        let root = tempfile::tempdir().unwrap();
        let shim = root.path().join("codex.cmd");
        std::fs::write(&shim, "@exit /b 42\r\n").unwrap();
        let output = spawn_with_fallback(
            &std::env::current_exe().unwrap(),
            &shim,
            &[
                "--exact",
                HELPER,
                "--ignored",
                "--nocapture",
                "--",
                "native",
            ],
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("CODEX_ARGS:[\"native\"]"));

        let invalid = root.path().join("codex.exe");
        std::fs::write(&invalid, "not an executable").unwrap();
        assert!(spawn_with_fallback(&invalid, &shim, &[]).is_err());
    }

    struct RunningChild(Child);
    impl Drop for RunningChild {
        fn drop(&mut self) {
            stop_codex(&mut self.0);
        }
    }

    #[test]
    fn stopping_npm_shim_terminates_its_descendant() {
        let root = tempfile::Builder::new()
            .prefix("codex tree test ")
            .tempdir()
            .unwrap();
        let mut child = RunningChild(
            spawn_with_fallback(
                &root.path().join("codex.exe"),
                &npm_shim(root.path()),
                &["wait-for-cleanup"],
            )
            .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(pid) = line.strip_prefix("CODEX_PID:") {
                    let _ = tx.send(pid.parse::<u32>().unwrap());
                    break;
                }
            }
        });
        let pid = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the shim should launch a descendant");
        assert_ne!(pid, child.0.id());
        // Keep a handle to the descendant so the assertion cannot mistake a
        // reused PID for the process we launched.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null(), "{}", io::Error::last_os_error());
        let descendant = unsafe { OwnedHandle::from_raw_handle(handle) };
        drop(child);
        assert_eq!(
            unsafe { WaitForSingleObject(descendant.as_raw_handle(), 10_000) },
            WAIT_OBJECT_0,
            "stopping the shim must also stop its app-server descendant"
        );
    }

    #[test]
    #[ignore = "subprocess helper for Windows Codex launcher tests"]
    fn cmd_child() {
        let args: Vec<_> = std::env::args()
            .skip_while(|arg| arg != "--")
            .skip(1)
            .collect();
        if args == ["wait-for-cleanup"] {
            println!("\nCODEX_PID:{}", std::process::id());
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(60));
        } else {
            println!("\nCODEX_ARGS:{}", serde_json::to_string(&args).unwrap());
        }
    }
}
