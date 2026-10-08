use crate::browser::Browser;
use crate::ollama::{self, arg_bool, arg_f64, arg_i64, arg_string, ChatMessage, ToolCall, ToolFunction};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

const OBSERVATION_TAG: &str = "[browser]";

#[derive(Debug, Deserialize)]
pub struct HistoryTurn {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    Status { text: String },
    Thinking { text: String },
    Content { text: String },
    ToolStart { name: String, args: String },
    ToolEnd {
        name: String,
        result: String,
        image: Option<String>,
    },
    Done,
    Error { text: String },
}

pub async fn run(
    app: AppHandle,
    settings: Settings,
    browser: &Mutex<Browser>,
    cancel: &AtomicBool,
    message: String,
    images: Vec<String>,
    history: Vec<HistoryTurn>,
) -> Result<(), String> {
    if crate::stack::uses_stack(&settings.model) || crate::gemini::uses_google(&settings.model) {
        if settings.gemma_api_key.is_empty() {
            let err = "Add your Gemma API key from Google AI Studio.".to_string();
            emit(&app, AgentEvent::Error { text: err.clone() });
            return Err(err);
        }
    } else if settings.api_key.is_empty() {
        let err = "Add your Ollama API key in the sidebar.".to_string();
        emit(&app, AgentEvent::Error { text: err.clone() });
        return Err(err);
    }
    let message = message.trim().to_string();
    let goal = if message.is_empty() {
        "Look at the attached image.".to_string()
    } else {
        message.clone()
    };
    if message.is_empty() && images.is_empty() {
        let err = "Say what you want done, or attach an image.".to_string();
        emit(&app, AgentEvent::Error { text: err.clone() });
        return Err(err);
    }

    let system = if crate::stack::uses_stack(&settings.model) {
        format!("{SYSTEM_PROMPT}\n\n{STACK_PROMPT}")
    } else {
        SYSTEM_PROMPT.to_string()
    };
    let mut messages = vec![ChatMessage::text("system", system)];
    let recent: Vec<HistoryTurn> = history.into_iter().rev().take(12).collect();
    for turn in recent.into_iter().rev() {
        if matches!(turn.role.as_str(), "user" | "assistant") && !turn.content.trim().is_empty() {
            let content: String = turn.content.chars().take(4000).collect();
            messages.push(ChatMessage::text(&turn.role, content));
        }
    }

    let mut user_images: Vec<String> = images
        .into_iter()
        .take(4)
        .map(|img| strip_data_url(&img))
        .filter(|img| !img.is_empty())
        .collect();
    let reference_image_count = user_images.len();
    if user_images.iter().any(|img| img.len() > 8_000_000) {
        let err = "One of those images is too large. Try a smaller one.".to_string();
        emit(&app, AgentEvent::Error { text: err.clone() });
        return Err(err);
    }

    emit(&app, AgentEvent::Status { text: "Checking Chrome…".into() });
    let connected = {
        let mut guard = browser.lock().await;
        guard.status().await.connected || guard.reattach().await.is_ok()
    };
    let mut user_text = if message.is_empty() {
        "Look at the attached image.".to_string()
    } else {
        message
    };
    if connected {
        emit(&app, AgentEvent::Status { text: "Scanning the whole Chrome page…".into() });
        let initial_observation = {
            let mut guard = browser.lock().await;
            match guard.scan_whole_page().await {
                Ok(observation) => Ok(observation),
                Err(_) => guard.observe().await,
            }
        };
        match initial_observation {
            Ok(obs) => {
                user_text = format!("{user_text}\n\n{OBSERVATION_TAG}\n{}", obs.text);
                emit(
                    &app,
                    AgentEvent::ToolEnd {
                        name: "browser_look".into(),
                        result: "Captured the current Chrome tab.".into(),
                        image: Some(obs.jpeg_b64.clone()),
                    },
                );
                user_images.push(obs.jpeg_b64);
            }
            Err(err) => {
                user_text = format!("{user_text}\n\n{OBSERVATION_TAG}\nThe screenshot failed: {err}. Call browser_look to try again.");
            }
        }
    } else {
        user_text.push_str(&format!(
            "\n\n{OBSERVATION_TAG}\nChrome is not attached, so browser tools will fail. Answer from what you know and any attached images, then call task_done. If the task needs the browser, tell the user to press Control my Chrome."
        ));
    }

    let task_message_index = messages.len();
    let mut user = ChatMessage::text("user", user_text);
    if !user_images.is_empty() {
        user.images = Some(user_images);
    }
    messages.push(user);

    let tools = tool_schema();
    let max_steps = settings.max_steps;
    let mut nudges = 0u32;
    let mut used_browser = false;
    let mut spoke = false;
    let mut last_action = String::new();
    let mut repeats = 0u32;
    let mut error_streak = 0u32;
    let mut reviewed_since_change = false;
    let mut next_vision = true;
    let mut next_dom = true;

    for step in 0..max_steps {
        if cancel.load(Ordering::SeqCst) {
            return stopped(&app);
        }
        trim_context(
            &mut messages,
            task_message_index,
            reference_image_count,
            step == 0,
        );
        let stuck = error_streak >= 2 || repeats >= 2;
        let model = if crate::stack::uses_stack(&settings.model) {
            crate::stack::orchestrator_model(stuck).to_string()
        } else {
            settings.model.clone()
        };
        let mut turn_messages = messages.clone();
        if crate::stack::uses_stack(&settings.model) {
            let page = if next_dom {
                last_browser_text(&messages)
            } else {
                String::new()
            };
            let shot = if next_vision {
                last_image(&messages)
            } else {
                None
            };
            let app_status = app.clone();
            let briefing = crate::stack::brief(
                &settings,
                &goal,
                &page,
                shot.as_deref(),
                cancel,
                |text| emit(&app_status, AgentEvent::Status { text: text.to_string() }),
            )
            .await;
            if !briefing.is_empty() {
                turn_messages.push(ChatMessage::text("user", briefing));
            }
        }
        next_vision = false;
        next_dom = false;
        emit(
            &app,
            AgentEvent::Status {
                text: format!(
                    "Step {} of {max_steps} · {}",
                    step + 1,
                    if crate::stack::uses_stack(&settings.model) {
                        if stuck {
                            "Gemini 4 Argon is taking over…"
                        } else {
                            "Gemini 3.5 Flash is orchestrating…"
                        }
                    } else if settings.think {
                        "Gemma is thinking…"
                    } else {
                        "Gemma is working…"
                    }
                ),
            },
        );

        let app_for_delta = app.clone();
        let turn = ollama::chat(
            &settings.api_key,
            &settings.gemma_api_key,
            &model,
            settings.think,
            &messages,
            &tools,
            |thinking, content| {
                if !thinking.is_empty() {
                    emit(&app_for_delta, AgentEvent::Thinking { text: thinking.to_string() });
                }
                if !content.is_empty() {
                    emit(&app_for_delta, AgentEvent::Content { text: content.to_string() });
                }
            },
            cancel,
        )
        .await;

        let turn = match turn {
            Ok(turn) => turn,
            Err(err) => {
                emit(&app, AgentEvent::Error { text: err.clone() });
                return Err(err);
            }
        };
        if cancel.load(Ordering::SeqCst) {
            return stopped(&app);
        }

        let mut calls = turn.tool_calls.clone();
        if calls.is_empty() {
            calls = parse_text_calls(&turn.content);
        }

        if calls.is_empty() {
            let said = !turn.content.trim().is_empty();
            spoke |= said;
            let limit = if used_browser { 3 } else { 1 };
            if connected && nudges < limit {
                nudges += 1;
                let mut assistant = ChatMessage::text("assistant", turn.content);
                if !turn.thinking.is_empty() {
                    assistant.thinking = Some(turn.thinking);
                }
                messages.push(assistant);
                messages.push(ChatMessage::text(
                    "user",
                    format!("{OBSERVATION_TAG}\nYou answered without calling a tool. Do not wait for the user or ask for permission. If the task is not finished, continue right now with the browser tools. If it is finished, call task_done with a short summary."),
                ));
                if said {
                    emit(&app, AgentEvent::Content { text: "\n\n".into() });
                }
                continue;
            }
            if !said && turn.thinking.trim().is_empty() && !spoke {
                let err = "Gemma returned an empty response.".to_string();
                emit(&app, AgentEvent::Error { text: err.clone() });
                return Err(err);
            }
            emit(&app, AgentEvent::Done);
            return Ok(());
        }

        let mut assistant = ChatMessage::text("assistant", turn.content.clone());
        if !turn.thinking.is_empty() {
            assistant.thinking = Some(turn.thinking);
        }
        assistant.tool_calls = Some(calls.clone());
        messages.push(assistant);

        let mut outcomes = Vec::new();
        let mut look_again = false;
        let mut scan_whole = false;
        let mut finished: Option<String> = None;
        for call in &calls {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let name = call.function.name.trim().to_string();
            if name == "task_done" {
                let summary = arg_string(&call.function.arguments, "summary").unwrap_or_default();
                outcomes.push((name, "Task marked done.".to_string()));
                finished = Some(summary);
                break;
            }
            let args_text = call.function.arguments.to_string();
            emit(
                &app,
                AgentEvent::ToolStart {
                    name: name.clone(),
                    args: truncate(&args_text, 600),
                },
            );
            if name.starts_with("browser_") {
                used_browser = true;
            }
            if changes_page(&name) {
                look_again = true;
            }
            if matches!(
                name.as_str(),
                "browser_scan"
                    | "browser_navigate"
                    | "browser_back"
                    | "browser_switch_tab"
                    | "browser_new_tab"
            ) {
                scan_whole = true;
            }
            let action_key = format!("{name}:{args_text}");
            if action_key == last_action {
                repeats += 1;
            } else {
                repeats = 0;
                last_action = action_key;
            }
            let is_final_action = if name == "browser_click" {
                browser
                    .lock()
                    .await
                    .is_final_action_target(
                        arg_i64(&call.function.arguments, "index"),
                        arg_f64(&call.function.arguments, "x"),
                        arg_f64(&call.function.arguments, "y"),
                    )
                    .await
                    .unwrap_or(false)
            } else {
                (name == "browser_type" && arg_bool(&call.function.arguments, "submit"))
                    || (name == "browser_press"
                        && arg_string(&call.function.arguments, "key")
                            .is_some_and(|key| key.eq_ignore_ascii_case("enter")))
            };
            let mut result = if is_final_action && !reviewed_since_change {
                Err("Final submission blocked: call browser_review first, compare every field and visible image against the user's original prompt, correct any mismatch, then submit.".into())
            } else if name == "wait" {
                let secs = arg_f64(&call.function.arguments, "seconds").unwrap_or(2.0).clamp(0.5, 15.0);
                sleep_cancellable(Duration::from_secs_f64(secs), cancel).await;
                look_again = true;
                Ok(format!("Waited {secs:.1} seconds."))
            } else {
                dispatch(browser, call).await
            };
            if result.is_ok() {
                if name == "browser_review" {
                    reviewed_since_change = true;
                } else if matches!(name.as_str(), "browser_type" | "browser_select")
                    || (name == "browser_click" && !is_final_action)
                {
                    reviewed_since_change = false;
                }
            }
            match &result {
                Ok(_) => error_streak = 0,
                Err(_) => error_streak += 1,
            }
            if repeats >= 2 {
                result = result.map(|text| {
                    format!("{text} Note: you have done this exact action {} times in a row. It is probably not working. Try a different element, scroll, use x,y coordinates, or another route.", repeats + 1)
                });
            }
            let mut text = match result {
                Ok(text) => text,
                Err(err) => format!("Error: {err}"),
            };
            if error_streak >= 3 {
                text.push_str(" Several actions failed in a row. Call browser_look, rethink the plan, and try a different approach.");
            }
            outcomes.push((name, text));
        }

        if let Some(summary) = finished {
            for (name, result) in outcomes.iter().filter(|(n, _)| n != "task_done") {
                emit(
                    &app,
                    AgentEvent::ToolEnd {
                        name: name.clone(),
                        result: result.clone(),
                        image: None,
                    },
                );
            }
            if turn.content.trim().is_empty() && !summary.trim().is_empty() {
                let prefix = if spoke { "\n\n" } else { "" };
                emit(&app, AgentEvent::Content { text: format!("{prefix}{}", summary.trim()) });
            }
            emit(&app, AgentEvent::Done);
            return Ok(());
        }
        spoke = false;

        if cancel.load(Ordering::SeqCst) {
            return stopped(&app);
        }

        let mut shot: Option<String> = None;
        let mut observed = None;
        if look_again {
            emit(
                &app,
                AgentEvent::Status {
                    text: if scan_whole {
                        "Scanning the whole page…".into()
                    } else {
                        "Looking at the page again…".into()
                    },
                },
            );
            let observation = {
                let mut guard = browser.lock().await;
                if scan_whole {
                    match guard.scan_whole_page().await {
                        Ok(observation) => Ok(observation),
                        Err(_) => guard.observe().await,
                    }
                } else {
                    guard.observe().await
                }
            };
            match observation {
                Ok(obs) => {
                    shot = Some(obs.jpeg_b64);
                    observed = Some(obs.text);
                }
                Err(err) => observed = Some(format!("Could not take a screenshot: {err}")),
            }
        }

        let last = outcomes.len().saturating_sub(1);
        for (index, (name, result)) in outcomes.into_iter().enumerate() {
            let image = if index == last { shot.clone() } else { None };
            emit(
                &app,
                AgentEvent::ToolEnd {
                    name: name.clone(),
                    result: result.clone(),
                    image,
                },
            );
            messages.push(ChatMessage {
                role: "tool".into(),
                content: result,
                thinking: None,
                images: None,
                tool_calls: None,
                tool_name: Some(name),
            });
        }

        if look_again {
            next_vision = true;
            next_dom = true;
        } else if scan_whole {
            next_dom = true;
        }

        if let Some(text) = observed {
            let mut follow = ChatMessage::text(
                "user",
                format!("{OBSERVATION_TAG}\nThis is the page after your last action (step {} of {max_steps}). Keep going with the original task without asking the user. Call task_done when it is fully finished.\n{text}", step + 1),
            );
            if let Some(image) = shot {
                follow.images = Some(vec![image]);
            }
            messages.push(follow);
        }
    }

    emit(
        &app,
        AgentEvent::Content {
            text: format!("\n\nI used all {max_steps} steps before finishing. Raise Max steps in the sidebar, or send \"keep going\" to continue."),
        },
    );
    emit(&app, AgentEvent::Done);
    Ok(())
}

fn stopped(app: &AppHandle) -> Result<(), String> {
    emit(app, AgentEvent::Status { text: "Stopped.".into() });
    emit(app, AgentEvent::Done);
    Ok(())
}

async fn sleep_cancellable(total: Duration, cancel: &AtomicBool) {
    let start = std::time::Instant::now();
    while start.elapsed() < total {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn dispatch(browser: &Mutex<Browser>, call: &ToolCall) -> Result<String, String> {
    let args = &call.function.arguments;
    let mut guard = browser.lock().await;
    match call.function.name.trim() {
        "browser_look" => Ok("Fresh screenshot taken.".into()),
        "browser_scan" => Ok("Full-page scan requested.".into()),
        "browser_click" => {
            let count = arg_i64(args, "click_count").unwrap_or(1).clamp(1, 3) as u32;
            let button = arg_string(args, "button").unwrap_or_else(|| "left".into());
            guard
                .click(arg_i64(args, "index"), arg_f64(args, "x"), arg_f64(args, "y"), &button, count)
                .await
        }
        "browser_type" => {
            let text = arg_string(args, "text").ok_or("browser_type needs text.")?;
            guard
                .type_text(
                    arg_i64(args, "index"),
                    arg_f64(args, "x"),
                    arg_f64(args, "y"),
                    &text,
                    arg_bool(args, "clear"),
                    arg_bool(args, "submit"),
                )
                .await
        }
        "browser_press" => {
            let key = arg_string(args, "key").ok_or("browser_press needs a key.")?;
            guard.press(&key).await
        }
        "browser_hover" => {
            guard
                .hover(arg_i64(args, "index"), arg_f64(args, "x"), arg_f64(args, "y"))
                .await
        }
        "browser_scroll" => {
            let dy = arg_f64(args, "dy").unwrap_or(600.0);
            let dx = arg_f64(args, "dx").unwrap_or(0.0);
            guard.scroll(dx, dy, arg_f64(args, "x"), arg_f64(args, "y")).await
        }
        "browser_navigate" => {
            let url = arg_string(args, "url").ok_or("browser_navigate needs a url.")?;
            guard.navigate(&url).await
        }
        "browser_back" => guard.back().await,
        "browser_select" => {
            let index = arg_i64(args, "index").ok_or("browser_select needs an index.")?;
            let label = arg_string(args, "label")
                .or_else(|| arg_string(args, "value"))
                .ok_or("browser_select needs a label.")?;
            guard.select_option(index, &label).await
        }
        "browser_read" => {
            let max = arg_i64(args, "max_chars").unwrap_or(6000).clamp(500, 20000) as usize;
            guard.read_text(max).await
        }
        "browser_review" => guard.review_form().await,
        "browser_tabs" => guard.tabs_text().await,
        "browser_switch_tab" => {
            let index = arg_i64(args, "index").ok_or("browser_switch_tab needs an index.")?;
            if index < 0 {
                return Err("Tab index cannot be negative.".into());
            }
            guard.switch_tab(index as usize).await
        }
        "browser_new_tab" => {
            let url = arg_string(args, "url").unwrap_or_default();
            guard.new_tab(&url).await
        }
        other => Err(format!("Unknown tool {other}. Use one of the listed tools.")),
    }
}

fn changes_page(name: &str) -> bool {
    matches!(
        name,
        "browser_look"
            | "browser_scan"
            | "browser_click"
            | "browser_type"
            | "browser_press"
            | "browser_hover"
            | "browser_scroll"
            | "browser_navigate"
            | "browser_back"
            | "browser_select"
            | "browser_review"
            | "browser_switch_tab"
            | "browser_new_tab"
    )
}

const KNOWN_TOOLS: &[&str] = &[
    "browser_look",
    "browser_scan",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_hover",
    "browser_scroll",
    "browser_navigate",
    "browser_back",
    "browser_select",
    "browser_read",
    "browser_review",
    "browser_tabs",
    "browser_switch_tab",
    "browser_new_tab",
    "wait",
    "task_done",
];

fn parse_text_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut offset = 0;
    while let Some(rel) = text[offset..].find('{') {
        let start = offset + rel;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) => {
                let consumed = stream.byte_offset();
                if let Some(call) = value_to_call(&value) {
                    calls.push(call);
                }
                offset = start + consumed.max(1);
            }
            _ => offset = start + 1,
        }
        if calls.len() >= 4 {
            break;
        }
    }
    calls
}

fn value_to_call(value: &Value) -> Option<ToolCall> {
    let obj = value.get("function").unwrap_or(value);
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if !KNOWN_TOOLS.contains(&name.as_str()) {
        return None;
    }
    let arguments = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("args"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some(ToolCall {
        kind: Some("function".into()),
        function: ToolFunction {
            index: None,
            name,
            arguments: ollama::normalize_arguments(arguments),
        },
    })
}

fn trim_context(
    messages: &mut Vec<ChatMessage>,
    task_message_index: usize,
    reference_image_count: usize,
    first_step: bool,
) {
    // A 777-step ceiling must not create a 777-step prompt. Keep the original
    // task plus a rolling window beginning at a browser observation.
    if messages.len() > 48 {
        let candidate = messages.len().saturating_sub(28);
        let start = (candidate..messages.len())
            .find(|index| {
                messages[*index].role == "user"
                    && messages[*index].content.contains(OBSERVATION_TAG)
            })
            .unwrap_or(candidate);
        if start > task_message_index + 1 {
            messages.drain(task_message_index + 1..start);
        }
    }

    let mut images_kept = 0;
    let mut thinking_kept = 0;
    let mut observations_kept = 0;
    for (index, message) in messages.iter_mut().enumerate().rev() {
        if message.images.as_ref().is_some_and(|imgs| !imgs.is_empty()) {
            if index == task_message_index {
                if !first_step {
                    if reference_image_count == 0 {
                        message.images = None;
                    } else if let Some(images) = &mut message.images {
                        images.truncate(reference_image_count);
                    }
                }
            } else if images_kept >= 1 {
                message.images = None;
            } else {
                images_kept += 1;
            }
        }
        if message.thinking.is_some() {
            thinking_kept += 1;
            if thinking_kept > 2 {
                message.thinking = None;
            }
        }
        if message.role == "user" {
            if let Some(pos) = message.content.find(OBSERVATION_TAG) {
                observations_kept += 1;
                if observations_kept > 2 && message.content.len() > pos + 400 {
                    let head = message.content[..pos].to_string();
                    let first_line: String = message.content[pos..]
                        .lines()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join("\n");
                    message.content = format!("{head}{first_line}\n(older page details removed)");
                }
            }
        }
        if message.role == "tool" && message.content.len() > 3000 && observations_kept > 2 {
            let cut: String = message.content.chars().take(800).collect();
            message.content = format!("{cut}…");
        }
    }
}

fn emit(app: &AppHandle, event: AgentEvent) {
    let _ = app.emit("agent-event", event);
}

fn strip_data_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((meta, data)) = trimmed.split_once(',') {
        if meta.contains("base64") {
            return data.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

fn tool_schema() -> Value {
    json!([
        tool("browser_look", "Take a fresh screenshot of the Chrome tab and list clickable elements.", json!({"type":"object","properties":{}})),
        tool("browser_scan", "Scan the entire page from top to bottom, including off-screen controls, whole-page text, and a full-page screenshot. Use this before acting when the page changed substantially or when you do not yet understand the complete page.", json!({"type":"object","properties":{}})),
        tool("browser_click", "Click something in the Chrome tab. Prefer index from the latest screenshot. Use x and y screenshot pixels when the target has no index.", json!({
            "type":"object",
            "properties":{
                "index":{"type":"integer","description":"Element index from the latest [browser] list"},
                "x":{"type":"number","description":"Screenshot pixel x, used when there is no index"},
                "y":{"type":"number","description":"Screenshot pixel y, used when there is no index"},
                "button":{"type":"string","enum":["left","right","middle"]},
                "click_count":{"type":"integer","description":"2 for a double click"}
            }
        })),
        tool("browser_type", "Click a form field and type text into it.", json!({
            "type":"object",
            "required":["text"],
            "properties":{
                "index":{"type":"integer"},
                "x":{"type":"number"},
                "y":{"type":"number"},
                "text":{"type":"string"},
                "clear":{"type":"boolean","description":"Replace the existing value"},
                "submit":{"type":"boolean","description":"Press Enter after typing"}
            }
        })),
        tool("browser_press", "Press a key such as Enter, Tab, Escape, Backspace, ArrowDown, or Space.", json!({
            "type":"object",
            "required":["key"],
            "properties":{"key":{"type":"string"}}
        })),
        tool("browser_hover", "Move the pointer over an element or screenshot point without clicking.", json!({
            "type":"object",
            "properties":{"index":{"type":"integer"},"x":{"type":"number"},"y":{"type":"number"}}
        })),
        tool("browser_scroll", "Scroll the page. Positive dy scrolls down.", json!({
            "type":"object",
            "properties":{"dx":{"type":"number"},"dy":{"type":"number"},"x":{"type":"number"},"y":{"type":"number"}}
        })),
        tool("browser_navigate", "Open an http or https URL in the current tab.", json!({
            "type":"object",
            "required":["url"],
            "properties":{"url":{"type":"string"}}
        })),
        tool("browser_back", "Go back in the tab history.", json!({"type":"object","properties":{}})),
        tool("browser_select", "Choose an option in a select element by visible label or value.", json!({
            "type":"object",
            "required":["index","label"],
            "properties":{"index":{"type":"integer"},"label":{"type":"string"}}
        })),
        tool("browser_read", "Read the full text of the current page, including parts below the screen. Use it to find answers or data on long pages.", json!({
            "type":"object",
            "properties":{"max_chars":{"type":"integer"}}
        })),
        tool("browser_review", "Required immediately before Submit, Send, Save, Confirm, Publish, payment, or any other final action. Read every current form value and final-action label, then compare them with the user's original prompt and reference images. Correct mismatches before the final click.", json!({
            "type":"object",
            "properties":{}
        })),
        tool("browser_tabs", "List open Chrome tabs.", json!({"type":"object","properties":{}})),
        tool("browser_switch_tab", "Switch to a tab by its index from browser_tabs.", json!({
            "type":"object",
            "required":["index"],
            "properties":{"index":{"type":"integer"}}
        })),
        tool("browser_new_tab", "Open a URL in a new Chrome tab and switch to it.", json!({
            "type":"object",
            "properties":{"url":{"type":"string"}}
        })),
        tool("wait", "Wait for a page to finish loading or an animation to end, then look again.", json!({
            "type":"object",
            "properties":{"seconds":{"type":"number"}}
        })),
        tool("task_done", "Call this once the whole task is finished, or if it truly cannot be done. The summary is shown to the user.", json!({
            "type":"object",
            "required":["summary"],
            "properties":{"summary":{"type":"string","description":"What you did and the result, or why it could not be done"}}
        }))
    ])
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

const SYSTEM_PROMPT: &str = r#"You are Gemma Work Bot, an autonomous agent that controls the user's own Google Chrome and finishes tasks end to end without asking for help.

You can see. Each [browser] message has a high-detail screenshot of the current tab, a numbered list of clickable elements, and visible page text. Gold number badges in the image match the indexes. Original reference images remain available throughout the task.

How to work:
- Work on your own until the task is completely done. Never stop to ask "should I continue?" or wait for confirmation. Pick sensible defaults and keep going.
- Before the first action on any new page, study the entire FULL PAGE SCAN from top to bottom. Understand the page's purpose, sections, forms, off-screen controls, and final actions before clicking anything. Navigation automatically produces a new full-page scan; call browser_scan again whenever the layout or task state is unclear.
- Do not click merely because an element looks plausible. First connect the action to the user's original goal and explain it in your private reasoning.
- Read the user's entire original prompt before planning. Inspect attached/reference images carefully: compare text, layout, colors, counts, selections, and other visible details instead of making a quick guess.
- Think about what is on screen, compare it to the original prompt and reference images, then act with a tool. Every reply should include a tool call until the task is finished.
- Prefer browser_click with an index. Use x and y only when the target is not listed. Coordinates are screenshot pixels from the top left.
- To fill a form, browser_type into each field (clear true to replace existing text) and use browser_select for dropdowns.
- REQUIRED BEFORE FINAL ACTION: before clicking Submit, Send, Save, Confirm, Finish, Publish, Post, payment, or creating an account, call browser_review. Re-read the original user prompt, compare every reported field and the fresh screenshot against it, and correct every mismatch. Only then perform the final action. Final actions are blocked until this review occurs.
- Do not use browser_type submit=true or press Enter to submit a final form before browser_review.
- If the page is loading, call wait. If something is below the screen, scroll. To read a long page, call browser_read.
- After each action you get a new screenshot. Trust it over your memory. If an action did nothing, try a different approach instead of repeating it.
- Close cookie banners and popups that block the page.
- When the whole task is finished, call task_done with a short summary of what you did and any answer the user asked for. Put the answer in the summary rather than writing it twice.
- Only stop early for things you cannot do yourself: a login with credentials you do not have, a CAPTCHA, two-factor codes, or missing personal information. Then call task_done and explain exactly what the user needs to do.
- Never pay, buy, delete accounts, or send messages to other people unless the user's request explicitly asks for that exact action.
- Messages that start with [browser] are automatic page updates, not the user."#;

const STACK_PROMPT: &str = r#"You are the cloud orchestrator in a Pure Google stack. Gemini 3.5 Flash handles tool calls. Gemini 4 Argon is used only when you are stuck. Local Ollama specialists may attach a [google-stack] note from PaliGemma 2 Mix (vision/OCR/coordinates), Gemma 3 4B (DOM/history), and Gemma 3 1B (ready/route). Trust [browser] indexes first. Use specialist coordinates only when no index exists."#;

fn last_browser_text(messages: &[ChatMessage]) -> String {
    for message in messages.iter().rev() {
        if message.content.contains(OBSERVATION_TAG) || message.content.contains("FULL PAGE SCAN") {
            return message.content.clone();
        }
    }
    messages.last().map(|message| message.content.clone()).unwrap_or_default()
}

fn last_image(messages: &[ChatMessage]) -> Option<String> {
    for message in messages.iter().rev() {
        if let Some(images) = &message.images {
            if let Some(image) = images.last().filter(|image| !image.is_empty()) {
                return Some(image.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_calls_written_as_text() {
        let text = "I'll click it.\n```json\n{\"name\": \"browser_click\", \"arguments\": {\"index\": 3}}\n```";
        let calls = parse_text_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "browser_click");
        assert_eq!(arg_i64(&calls[0].function.arguments, "index"), Some(3));
    }

    #[test]
    fn ignores_plain_json() {
        assert!(parse_text_calls("{\"name\": \"Ada\"}").is_empty());
    }
}
