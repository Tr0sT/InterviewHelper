use interview_helper::{
    audio,
    codex::Codex,
    platform::{self, control, hotkeys::Hotkeys},
    service::Service,
    session::Settings,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
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
    platform::capture::devices()
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
    platform::open_path(&state.service.context_file()?)
}
#[tauri::command]
fn open_data_folder(state: State<AppState>) -> Result<(), String> {
    platform::open_path(&state.service.root)
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
    let args: Vec<_> = std::env::args().collect();
    if args.len() > 1 {
        if args.len() != 3 || args[1] != "--command" {
            return Err(
                "Использование: interview-helper [--command show|cut|action:<id>|start|stop|status|quit]".into(),
            );
        }
        let reply = control::send(&args[2])?;
        if let Some(error) = reply["error"].as_str() {
            return Err(error.into());
        }
        println!("{reply}");
        return Ok(());
    }
    if control::send("show").is_ok() {
        return Ok(());
    }
    let control = Arc::new(control::Server::bind()?);
    let incoming = control.clone();
    let root = platform::data_dir()?;
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
            incoming.start(move |command| match handle_command(&handle, command) {
                Ok(value) => value,
                Err(error) => {
                    handle.state::<AppState>().service.set_error(error.clone());
                    json!({"error":error})
                }
            })?;
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
    drop(control);
    Ok(())
}
