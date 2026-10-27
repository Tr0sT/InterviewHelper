use interview_helper::{
    audio, codex::Codex, hotkeys::Hotkeys, service::Service, session::Settings,
};
use serde_json::{json, Value};
use std::{
    fs::{self},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, State,
};

struct AppState {
    service: Arc<Service>,
    codex: Arc<Codex>,
    hotkeys: Hotkeys,
    hotkey_error: Mutex<Option<String>>,
}

#[tauri::command]
fn snapshot(state: State<AppState>) -> Value {
    let mut result = state.service.snapshot();
    result["hotkey_error"] = json!(state
        .hotkey_error
        .lock()
        .expect("hotkey mutex poisoned")
        .clone());
    result
}
#[tauri::command]
fn audio_devices() -> Result<audio::Devices, String> {
    audio::devices()
}
#[tauri::command]
fn start_recording(state: State<AppState>) -> Result<(), String> {
    state.service.start()
}
#[tauri::command]
fn stop_recording(state: State<AppState>) -> Result<(), String> {
    state.service.stop()
}
#[tauri::command]
fn submit_segment(action_id: Option<String>, state: State<AppState>) -> Result<(), String> {
    state.service.cut(action_id.as_deref())
}
#[tauri::command]
fn retry_segment(seq: u64, state: State<AppState>) -> Result<(), String> {
    state.service.retry(seq)
}
#[tauri::command]
fn regenerate_answer(
    session_id: String,
    answer_id: u64,
    state: State<AppState>,
) -> Result<(), String> {
    state.service.regenerate_answer(&session_id, answer_id)
}
#[tauri::command]
fn save_model_settings(
    model: String,
    reasoning_effort: String,
    speed: String,
    state: State<AppState>,
) -> Result<(), String> {
    state
        .service
        .save_model_settings(model, reasoning_effort, speed)
}
#[tauri::command]
fn select_session(id: String, state: State<AppState>) -> Result<(), String> {
    state.service.select(&id)
}
#[tauri::command]
fn save_settings(settings: Settings, state: State<AppState>) -> Result<(), String> {
    settings.validate()?;
    let previous = state.service.settings();
    state.hotkeys.replace(&settings)?;
    if let Err(error) = state.service.save_settings(settings) {
        let _ = state.hotkeys.replace(&previous);
        return Err(error);
    }
    *state.hotkey_error.lock().map_err(|e| e.to_string())? = None;
    Ok(())
}
#[tauri::command]
async fn check_codex(state: State<'_, AppState>) -> Result<Value, String> {
    let codex = state.codex.clone();
    tauri::async_runtime::spawn_blocking(move || codex.check())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn clear_error(state: State<AppState>) {
    state.service.clear_error();
}
#[tauri::command]
fn open_context_file(state: State<AppState>) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(state.service.context_file()?)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
#[tauri::command]
fn open_data_folder(state: State<AppState>) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(&state.service.root)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

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
fn send_command(path: &PathBuf, command: &str) -> Result<Value, String> {
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
fn handle_command(app: &tauri::AppHandle, command: &str) -> Result<Value, String> {
    let state = app.state::<AppState>();
    match command {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                window.show().map_err(|e| e.to_string())?;
                window.set_focus().map_err(|e| e.to_string())?;
            }
        }
        "cut" => state.service.cut(None)?,
        action if action.starts_with("action:") => state.service.cut(Some(&action[7..]))?,
        "start" => state.service.start()?,
        "stop" => state.service.stop()?,
        "status" => {
            let mut result = state.service.snapshot();
            result["hotkey_error"] = json!(state
                .hotkey_error
                .lock()
                .map_err(|e| e.to_string())?
                .clone());
            return Ok(result);
        }
        "quit" => {
            if state.service.busy() {
                return Err("Запись или обработка ещё выполняются.".into());
            }
            app.exit(0);
        }
        _ => return Err("Неизвестная команда.".into()),
    }
    Ok(json!({"ok":true}))
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Interview Helper: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let socket = socket_path()?;
    let args: Vec<_> = std::env::args().collect();
    if args.len() > 1 {
        if args.len() != 3 || args[1] != "--command" {
            return Err(
                "Использование: interview-helper [--command show|cut|action:<id>|start|stop|status|quit]".into(),
            );
        }
        let reply = send_command(&socket, &args[2])?;
        if let Some(error) = reply["error"].as_str() {
            return Err(error.into());
        }
        println!("{reply}");
        return Ok(());
    }
    if send_command(&socket, "show").is_ok() {
        return Ok(());
    }
    if socket.exists() {
        let meta = fs::symlink_metadata(&socket).map_err(|e| e.to_string())?;
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err("Путь управляющего сокета занят другим файлом.".into());
        }
        fs::remove_file(&socket).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let root = if let Some(path) = std::env::var_os("INTERVIEW_HELPER_DATA_DIR") {
        PathBuf::from(path)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").expect("HOME is required"))
                    .join(".local/share")
            })
            .join("interview-helper")
    };
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            snapshot,
            audio_devices,
            start_recording,
            stop_recording,
            submit_segment,
            retry_segment,
            regenerate_answer,
            save_model_settings,
            select_session,
            save_settings,
            check_codex,
            clear_error,
            open_context_file,
            open_data_folder
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let notify = Arc::new(move |event: &str, value: Value| {
                let _ = handle.emit(event, value);
            });
            let codex = Arc::new(Codex);
            let service = Service::new(root, codex.clone(), notify)?;
            let hotkeys = Hotkeys::new(std::env::current_exe()?);
            let hotkey_error = hotkeys.replace(&service.settings()).err();
            app.manage(AppState {
                service,
                codex,
                hotkeys,
                hotkey_error: Mutex::new(hotkey_error),
            });
            let open =
                MenuItem::with_id(app, "open", "Открыть Interview Helper", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::new()
                .icon(
                    app.default_window_icon()
                        .ok_or("Нет иконки приложения.")?
                        .clone(),
                )
                .tooltip("Interview Helper")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "open" {
                        let _ = handle_command(app, "show");
                    }
                    if event.id.as_ref() == "quit" {
                        let state = app.state::<AppState>();
                        if state.service.busy() {
                            state.service.set_error(
                                "Сначала остановите запись и дождитесь завершения задач.".into(),
                            );
                            let _ = handle_command(app, "show");
                        } else {
                            app.exit(0);
                        }
                    }
                })
                .build(app)?;
            let handle = app.handle().clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut command = String::new();
                    let read = BufReader::new((&stream).take(129)).read_line(&mut command);
                    let reply = if read.is_ok() && command.len() < 128 {
                        match handle_command(&handle, command.trim()) {
                            Ok(value) => value,
                            Err(error) => {
                                handle.state::<AppState>().service.set_error(error.clone());
                                json!({"error":error})
                            }
                        }
                    } else {
                        json!({"error":"Некорректная команда."})
                    };
                    let _ = writeln!(stream, "{reply}");
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .map_err(|e| e.to_string())?;
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            handle.state::<AppState>().hotkeys.clear();
        }
    });
    let _ = fs::remove_file(socket);
    Ok(())
}
