mod agent;
mod browser;
mod gemini;
mod ollama;
mod settings;
mod stack;

use agent::HistoryTurn;
use browser::Browser;
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

struct AppState {
    settings_path: PathBuf,
    profile_dir: PathBuf,
    settings: Mutex<Settings>,
    browser: Mutex<Browser>,
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.settings.lock().await.clone())
}

#[tauri::command]
async fn save_settings(
    state: State<'_, AppState>,
    api_key: String,
    gemma_api_key: String,
    ollama_host: String,
    model: String,
    think: bool,
    debug_port: u16,
    max_steps: u32,
) -> Result<Settings, String> {
    let settings = Settings {
        api_key,
        gemma_api_key,
        ollama_host,
        model,
        think,
        debug_port,
        max_steps,
    }
    .normalized();
    settings.save(&state.settings_path)?;
    state.browser.lock().await.set_port(settings.debug_port);
    *state.settings.lock().await = settings.clone();
    Ok(settings)
}

#[tauri::command]
async fn browser_status(state: State<'_, AppState>) -> Result<browser::BrowserStatus, String> {
    if let Ok(browser) = state.browser.try_lock() {
        return Ok(browser.status().await);
    }
    let settings = state.settings.lock().await.clone();
    Ok(browser::BrowserStatus {
        connected: true,
        port: settings.debug_port,
        chrome_running: true,
        debug_open: true,
        tabs: Vec::new(),
        active_title: Some("Work Bot is using Chrome".into()),
        active_url: None,
    })
}

#[tauri::command]
async fn launch_chrome(state: State<'_, AppState>, mode: String) -> Result<browser::BrowserStatus, String> {
    let settings = state.settings.lock().await.clone();
    let profile = state.profile_dir.clone();
    let mut browser = state.browser.lock().await;
    browser.set_port(settings.debug_port);
    browser.launch(&mode, &profile).await
}

#[tauri::command]
async fn stop_agent(state: State<'_, AppState>) -> Result<(), String> {
    state.cancel.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
async fn run_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    message: String,
    images: Vec<String>,
    history: Vec<HistoryTurn>,
) -> Result<(), String> {
    if state
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("A task is already running.".into());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let settings = state.settings.lock().await.clone();
    let cancel = state.cancel.clone();
    let running = state.running.clone();
    let _ = app.emit(
        "agent-event",
        agent::AgentEvent::Status {
            text: "Preparing the bot browser…".into(),
        },
    );
    {
        let mut browser = state.browser.lock().await;
        browser.set_port(settings.debug_port);
        if let Err(err) = browser.launch("auto", &state.profile_dir).await {
            running.store(false, Ordering::SeqCst);
            let _ = app.emit(
                "agent-event",
                agent::AgentEvent::Error { text: err.clone() },
            );
            return Err(err);
        }
    }
    let result = agent::run(
        app,
        settings,
        &state.browser,
        cancel.as_ref(),
        message,
        images,
        history,
    )
    .await;
    running.store(false, Ordering::SeqCst);
    result
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app
                .handle()
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("gemma-work-bot"));
            std::fs::create_dir_all(&dir).ok();
            let settings_path = dir.join("settings.json");
            let settings = Settings::load(&settings_path).normalized();
            let port = settings.debug_port;
            app.manage(AppState {
                profile_dir: dir.join("chrome-profile"),
                settings_path,
                settings: Mutex::new(settings),
                browser: Mutex::new(Browser::new(port)),
                running: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            browser_status,
            launch_chrome,
            run_agent,
            stop_agent
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
