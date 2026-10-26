#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use desktop_core::remote::{RemoteAccess, RemoteAsset};
use desktop_core::sms::SmsConnector;
use desktop_core::{
    control_cli,
    local_control::{self, LocalControl},
    Desktop,
};
use serde_json::Value;
use std::ffi::OsStr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

struct AppState {
    desktop: Arc<Desktop>,
    remote: Arc<RemoteAccess>,
    sms: Arc<SmsConnector>,
    owner_control: Arc<LocalControl>,
    closing: Arc<AtomicBool>,
    shutdown_complete: AtomicBool,
}

#[tauri::command]
async fn desktop_request(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    method: String,
    args: Value,
) -> Result<Value, String> {
    if state.closing.load(Ordering::SeqCst) {
        return Err("The application is shutting down".into());
    }
    if matches!(
        method.as_str(),
        "remote_status" | "remote_configure" | "remote_token" | "remote_rotate_token"
    ) {
        return state
            .remote
            .request(&method, args)
            .await
            .map_err(|e| format!("{e:#}"));
    }
    if method.starts_with("sms_") {
        return state
            .sms
            .request(&method, args)
            .await
            .map_err(|error| format!("{error:#}"));
    }
    if method == "pick_directory" {
        let selected = tauri::async_runtime::spawn_blocking(move || {
            app.dialog().file().blocking_pick_folder()
        })
        .await
        .map_err(|e| e.to_string())?;
        return selected
            .map(|path| {
                path.into_path()
                    .map(|p| Value::String(p.to_string_lossy().into()))
                    .map_err(|e| e.to_string())
            })
            .unwrap_or(Ok(Value::Null));
    }
    if method == "open_external" {
        let url = args["url"].as_str().ok_or("Missing URL")?;
        let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("Only HTTP(S) links without embedded credentials may be opened".into());
        }
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| e.to_string())?;
        return Ok(Value::Null);
    }
    state
        .desktop
        .request(&method, args)
        .await
        .map_err(|e| format!("{e:#}"))
}
async fn shutdown_host(state: &AppState) -> Result<(), String> {
    state
        .sms
        .suspend()
        .await
        .map_err(|error| format!("SMS connector: {error:#}"))?;
    let remote = state.remote.suspend().await;
    let bots = state.desktop.shutdown_all().await;
    match (remote, bots) {
        (Ok(()), Ok(())) => {
            state
                .remote
                .shutdown()
                .await
                .map_err(|error| format!("Remote access: {error:#}"))?;
            state.owner_control.shutdown().await;
            state
                .sms
                .shutdown()
                .await
                .map_err(|error| format!("SMS connector: {error:#}"))?;
            Ok(())
        }
        (Err(error), Ok(())) => Err(format!("Remote access: {error:#}")),
        (Ok(()), Err(error)) => Err(format!("Agent shutdown: {error:#}")),
        (Err(remote), Err(bots)) => Err(format!(
            "Remote access: {remote:#}\nAgent shutdown: {bots:#}"
        )),
    }
}
fn request_quit(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.closing.swap(true, Ordering::SeqCst) {
        return;
    }
    let desktop = state.desktop.clone();
    let remote = state.remote.clone();
    tauri::async_runtime::spawn(async move {
        let proceed = if desktop.has_active_work()
            || remote.is_running()
            || app.state::<AppState>().sms.is_enabled()
        {
            let dialog_app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message("Running agents, remote access, and SMS connections will be stopped. Saved conversations remain available, but interrupted prompts will not be replayed.")
                    .title("Stop agents and quit?")
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::YesNo)
                    .blocking_show()
            })
            .await
            .unwrap_or(false)
        } else {
            true
        };
        if !proceed {
            app.state::<AppState>()
                .closing
                .store(false, Ordering::SeqCst);
            return;
        }
        if let Err(error) = shutdown_host(&app.state::<AppState>()).await {
            if let Err(resume_error) = app.state::<AppState>().sms.resume().await {
                eprintln!("Could not resume the SMS connector after failed quit: {resume_error:#}");
            }
            app.state::<AppState>()
                .closing
                .store(false, Ordering::SeqCst);
            let dialog_app = app.clone();
            let message = format!("Could not stop the desktop cleanly: {error}\n\nThe app remains open so you can inspect the bots and retry.");
            let _ = tauri::async_runtime::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(message)
                    .title("Shutdown needs attention")
                    .kind(MessageDialogKind::Error)
                    .blocking_show()
            })
            .await;
            return;
        }
        app.state::<AppState>()
            .shutdown_complete
            .store(true, Ordering::SeqCst);
        app.exit(0);
    });
}
fn main() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() == Some(OsStr::new("remote")) {
        let result =
            local_control::data_directory(env!("OMP_DESKTOP_IDENTIFIER")).and_then(|directory| {
                tauri::async_runtime::block_on(control_cli::run(arguments.collect(), directory))
            });
        match result {
            Ok(output) => print!("{output}"),
            Err(error) => {
                eprintln!("AMP Desktop remote: {error:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .menu(|app| {
            use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
            let menu = Menu::default(app)?;
            #[cfg(target_os = "macos")]
            {
                // The native predefined Quit calls NSApp.terminate directly,
                // bypassing RunEvent::ExitRequested and its confirmation gate.
                let application_menu = Submenu::with_items(
                    app,
                    "AMP Desktop",
                    true,
                    &[
                        &PredefinedMenuItem::about(app, None, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::services(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::hide(app, None)?,
                        &PredefinedMenuItem::hide_others(app, None)?,
                        &PredefinedMenuItem::show_all(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &MenuItem::with_id(
                            app,
                            "quit",
                            "Quit AMP Desktop",
                            true,
                            Some("CmdOrCtrl+Q"),
                        )?,
                    ],
                )?;
                menu.remove_at(0)?;
                menu.insert(&application_menu, 0)?;
            }
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "quit" {
                request_quit(app.clone());
            }
        })
        .setup(|app| {
            let directory = local_control::data_directory(env!("OMP_DESKTOP_IDENTIFIER"))?;
            let desktop = tauri::async_runtime::block_on(Desktop::open(directory.clone()))?;
            let resolver = app.asset_resolver();
            let assets = Arc::new(move |path: &str| {
                resolver.get(path.to_owned()).map(|asset| RemoteAsset {
                    bytes: asset.bytes,
                    mime_type: asset.mime_type,
                })
            });
            let remote = tauri::async_runtime::block_on(RemoteAccess::open(
                desktop.clone(),
                directory.clone(),
                assets,
            ))?;
            let sms = tauri::async_runtime::block_on(SmsConnector::open(
                desktop.clone(),
                directory.clone(),
            ))?;
            let closing = remote.closing_flag();
            let owner_control = tauri::async_runtime::block_on(LocalControl::start(
                directory,
                remote.clone(),
                closing.clone(),
            ))?;
            app.manage(AppState {
                desktop,
                remote,
                sms,
                owner_control,
                closing,
                shutdown_complete: AtomicBool::new(false),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![desktop_request])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !window
                    .state::<AppState>()
                    .shutdown_complete
                    .load(Ordering::SeqCst)
                {
                    api.prevent_close();
                    if window.state::<AppState>().remote.is_running()
                        || window.state::<AppState>().sms.is_enabled()
                    {
                        if let Err(error) = window.hide() {
                            eprintln!("Could not hide the hosting window: {error}");
                        }
                    } else {
                        request_quit(window.app_handle().clone());
                    }
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Failed to initialize AMP Desktop");
    app.run(|app, event| match event {
        tauri::RunEvent::ExitRequested { api, .. }
            if !app
                .state::<AppState>()
                .shutdown_complete
                .load(Ordering::SeqCst) =>
        {
            api.prevent_exit();
            request_quit(app.clone());
        }
        tauri::RunEvent::Exit => {
            let state = app.state::<AppState>();
            if !state.shutdown_complete.load(Ordering::SeqCst) {
                // OS-driven termination may bypass the cancellable quit paths.
                state.closing.store(true, Ordering::SeqCst);
                if let Err(error) = tauri::async_runtime::block_on(shutdown_host(&state)) {
                    eprintln!("Could not stop the desktop before OS termination: {error}");
                }
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if let Some(window) = app.get_webview_window("main") {
                if let Err(error) = window.show().and_then(|_| window.set_focus()) {
                    eprintln!("Could not restore the hosting window: {error}");
                }
            }
        }
        _ => {}
    });
}
