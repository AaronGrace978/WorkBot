mod agent;
mod browser;
mod documents;
mod gemini;
mod memory;
mod ollama;
mod settings;
mod stack;

use agent::HistoryTurn;
use browser::Browser;
use documents::{ApplicantProfile, DocumentSnapshot, DocumentVault};
use memory::{MemorySnapshot, MemoryVault};
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;

struct AppState {
    settings_path: PathBuf,
    memory_path: PathBuf,
    documents_path: PathBuf,
    documents_dir: PathBuf,
    profile_dir: PathBuf,
    settings: Mutex<Settings>,
    memory: Mutex<MemoryVault>,
    documents: Mutex<DocumentVault>,
    browser: Mutex<Browser>,
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.settings.lock().await.clone())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn save_settings(
    state: State<'_, AppState>,
    api_key: String,
    gemma_api_key: String,
    ollama_host: String,
    model: String,
    think: bool,
    debug_port: u16,
    max_steps: u32,
    auto_learn: Option<bool>,
    theme: Option<String>,
) -> Result<Settings, String> {
    let prev = state.settings.lock().await.clone();
    let settings = Settings {
        api_key,
        gemma_api_key,
        ollama_host,
        model,
        think,
        debug_port,
        max_steps,
        auto_learn: auto_learn.unwrap_or(prev.auto_learn),
        theme: theme.unwrap_or(prev.theme),
    }
    .normalized();
    settings.save(&state.settings_path)?;
    state.browser.lock().await.set_port(settings.debug_port);
    *state.settings.lock().await = settings.clone();
    Ok(settings)
}

#[tauri::command]
async fn get_memories(state: State<'_, AppState>) -> Result<MemorySnapshot, String> {
    Ok(state.memory.lock().await.snapshot())
}

#[tauri::command]
async fn save_memory(
    state: State<'_, AppState>,
    kind: String,
    domain: String,
    title: String,
    content: String,
    tags: Vec<String>,
) -> Result<MemorySnapshot, String> {
    if title.trim().is_empty() || content.trim().is_empty() {
        return Err("Provide both a title and content for the memory.".into());
    }
    let mut vault = state.memory.lock().await;
    vault.upsert(&kind, &domain, &title, &content, tags, "user");
    vault.save(&state.memory_path)?;
    Ok(vault.snapshot())
}

#[tauri::command]
async fn delete_memory(state: State<'_, AppState>, id: String) -> Result<MemorySnapshot, String> {
    let mut vault = state.memory.lock().await;
    vault.delete(&id);
    vault.save(&state.memory_path)?;
    Ok(vault.snapshot())
}

#[tauri::command]
async fn clear_learned_memories(state: State<'_, AppState>) -> Result<MemorySnapshot, String> {
    let mut vault = state.memory.lock().await;
    vault.clear_learned();
    vault.save(&state.memory_path)?;
    Ok(vault.snapshot())
}

#[tauri::command]
async fn get_documents(state: State<'_, AppState>) -> Result<DocumentSnapshot, String> {
    Ok(state.documents.lock().await.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn save_applicant_profile(
    state: State<'_, AppState>,
    profile: ApplicantProfile,
) -> Result<DocumentSnapshot, String> {
    let mut docs = state.documents.lock().await;
    docs.set_profile(profile);
    docs.save(&state.documents_path)?;
    Ok(docs.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn add_document(
    state: State<'_, AppState>,
    name: String,
    kind: String,
    data_base64: String,
) -> Result<DocumentSnapshot, String> {
    let bytes = documents::decode_upload(&data_base64)?;
    let mut docs = state.documents.lock().await;
    docs.add_upload(&state.documents_dir, &name, &kind, &bytes)?;
    docs.save(&state.documents_path)?;
    Ok(docs.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn update_document_text(
    state: State<'_, AppState>,
    id: String,
    text: String,
) -> Result<DocumentSnapshot, String> {
    let mut docs = state.documents.lock().await;
    docs.update_text(&id, &text)?;
    docs.save(&state.documents_path)?;
    Ok(docs.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn delete_document(state: State<'_, AppState>, id: String) -> Result<DocumentSnapshot, String> {
    let mut docs = state.documents.lock().await;
    docs.delete(&state.documents_dir, &id)?;
    docs.save(&state.documents_path)?;
    Ok(docs.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn preview_document(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let path = state
        .documents
        .lock()
        .await
        .preview_path(&state.documents_dir, &id)?;
    let text = path.to_string_lossy().to_string();
    app.opener()
        .open_path(text, None::<&str>)
        .map_err(|e| format!("Could not open that document: {e}"))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn draft_document(
    state: State<'_, AppState>,
    kind: String,
    job_title: String,
    company: String,
    job_text: String,
    source_id: Option<String>,
    notes: Option<String>,
) -> Result<DocumentSnapshot, String> {
    let settings = state.settings.lock().await.clone();
    if stack::uses_stack(&settings.model) || gemini::uses_google(&settings.model) {
        if settings.gemma_api_key.is_empty() {
            return Err("Add your Google AI Studio key in the sidebar first.".into());
        }
    } else if settings.api_key.is_empty() {
        return Err("Add your Ollama API key in the sidebar first.".into());
    }
    let kind = documents::normalize_draft_kind(&kind);
    let (profile, source) = {
        let docs = state.documents.lock().await;
        if !docs.has_material(source_id.as_deref()) {
            return Err("Add a resume or fill in your summary before drafting.".into());
        }
        (docs.profile().clone(), docs.source_material(source_id.as_deref()))
    };
    let mut job_text = job_text.trim().to_string();
    if job_text.is_empty() {
        if let Ok(mut guard) = state.browser.try_lock() {
            if let Ok(page_text) = guard.read_text(8000).await {
                job_text = page_text;
            }
        }
    }
    let cancel = AtomicBool::new(false);
    let body = documents::write_body(
        &settings,
        &profile,
        &source,
        &kind,
        &job_title,
        &company,
        &job_text,
        notes.as_deref().unwrap_or(""),
        &cancel,
    )
    .await?;
    let mut composed = documents::compose_document(&profile, &kind, &body, &job_title, &company);
    if let Ok(mut guard) = state.browser.try_lock() {
        if let Ok(pdf) = guard.render_html_pdf(&composed.html).await {
            composed.pdf = pdf;
        }
    }
    let mut docs = state.documents.lock().await;
    docs.store_generated(&state.documents_dir, &composed, &kind, &job_title, &company)?;
    docs.save(&state.documents_path)?;
    Ok(docs.snapshot(&state.documents_dir))
}

#[tauri::command]
async fn attach_document_now(
    state: State<'_, AppState>,
    id: String,
    method: Option<String>,
    format: Option<String>,
) -> Result<String, String> {
    if state.running.load(Ordering::SeqCst) {
        return Err("Wait until the current run finishes.".into());
    }
    let kind = state.documents.lock().await.kind_of(&id)?;
    let targets = state
        .browser
        .lock()
        .await
        .file_targets()
        .await
        .map_err(|_| "Start bot Chrome and open the application page first.".to_string())?;
    let chosen_input = browser::choose_file_input(&targets, &kind, None).ok().cloned();
    let accept = chosen_input
        .as_ref()
        .map(|target| target.accept.as_str())
        .unwrap_or("");
    let path = {
        let docs = state.documents.lock().await;
        let fmt = docs.select_format(&id, format.as_deref(), accept)?;
        docs.absolute(&state.documents_dir, &id, &fmt)?
    };
    let mut guard = state.browser.lock().await;
    match method.unwrap_or_else(|| "auto".into()).to_lowercase().as_str() {
        "drop" => {
            let drop = targets
                .iter()
                .find(|target| target.kind == "drop")
                .ok_or("No drop zone is visible on the current page.")?;
            guard.drop_file(Some(drop.index), None, None, &path).await
        }
        "upload" => {
            let target = browser::choose_file_input(&targets, &kind, None)?;
            guard.upload_file(target.index, &path).await
        }
        _ => {
            if let Some(target) = chosen_input {
                if let Ok(done) = guard.upload_file(target.index, &path).await {
                    return Ok(done);
                }
            }
            if let Some(drop) = targets.iter().find(|target| target.kind == "drop") {
                return guard.drop_file(Some(drop.index), None, None, &path).await;
            }
            Err("No file input or drop zone was found on the active Chrome tab.".into())
        }
    }
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
async fn browser_quick_action(
    state: State<'_, AppState>,
    action: String,
    value: String,
) -> Result<browser::BrowserStatus, String> {
    let mut browser = state.browser.lock().await;
    match action.as_str() {
        "switch_tab" => {
            if let Ok(idx) = value.trim().parse::<usize>() {
                browser.switch_tab(idx).await?;
            }
        }
        "new_tab" => {
            browser.new_tab(&value).await?;
        }
        "navigate" => {
            browser.navigate(&value).await?;
        }
        "reload" => {
            browser.reload().await?;
        }
        _ => return Err("Unknown browser action.".into()),
    }
    Ok(browser.status().await)
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
        &state.memory,
        &state.memory_path,
        &state.documents,
        &state.documents_path,
        &state.documents_dir,
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
            let memory_path = dir.join("memory.json");
            let documents_path = dir.join("documents.json");
            let documents_dir = dir.join("documents");
            std::fs::create_dir_all(&documents_dir).ok();
            let settings = Settings::load(&settings_path).normalized();
            let memory = MemoryVault::load(&memory_path);
            let _ = memory.save(&memory_path);
            let documents = DocumentVault::load(&documents_path);
            let _ = documents.save(&documents_path);
            let port = settings.debug_port;
            app.manage(AppState {
                profile_dir: dir.join("chrome-profile"),
                settings_path,
                memory_path,
                documents_path,
                documents_dir,
                settings: Mutex::new(settings),
                memory: Mutex::new(memory),
                documents: Mutex::new(documents),
                browser: Mutex::new(Browser::new(port)),
                running: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_memories,
            save_memory,
            delete_memory,
            clear_learned_memories,
            get_documents,
            save_applicant_profile,
            add_document,
            update_document_text,
            delete_document,
            preview_document,
            draft_document,
            attach_document_now,
            browser_status,
            launch_chrome,
            browser_quick_action,
            run_agent,
            stop_agent
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
