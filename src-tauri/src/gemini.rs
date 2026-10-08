use crate::ollama::{ChatMessage, ModelTurn, ToolCall, ToolFunction};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize, Default)]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<FunctionCall>,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

pub fn uses_google(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("gemma-") || model.starts_with("gemini-")
}

pub async fn chat<F>(
    api_key: &str,
    model: &str,
    think: bool,
    messages: &[ChatMessage],
    tools: &Value,
    mut on_delta: F,
    cancel: &AtomicBool,
) -> Result<ModelTurn, String>
where
    F: FnMut(&str, &str),
{
    if api_key.is_empty() {
        return Err("Add your Gemma API key from Google AI Studio.".into());
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let body = request_body(model, messages, tools, think);
    let url = format!(
        "{API_BASE}/{}:streamGenerateContent?alt=sse",
        urlencoding(model.trim())
    );

    let mut attempt = 0u32;
    let response = loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(empty_turn());
        }
        let sent = tokio::time::timeout(
            Duration::from_secs(90),
            client
                .post(&url)
                .header("x-goog-api-key", api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send(),
        )
        .await;
        let retry_reason = match sent {
            Err(_) => "Google Gemma API did not answer in 90 seconds".to_string(),
            Ok(Err(e)) => format!("Could not reach Google Gemma API: {e}"),
            Ok(Ok(resp)) if resp.status().is_success() => break resp,
            Ok(Ok(resp)) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let detail: String = text.chars().take(400).collect();
                let message = format!("Google Gemma API returned {status}: {detail}");
                if status.as_u16() != 429 && !status.is_server_error() {
                    return Err(message);
                }
                message
            }
        };
        attempt += 1;
        if attempt > 3 {
            return Err(retry_reason);
        }
        let wait = Duration::from_secs(2u64.pow(attempt));
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if cancel.load(Ordering::SeqCst) {
                return Ok(empty_turn());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut pending_think = String::new();
    let mut pending_content = String::new();
    let mut last_flush = Instant::now();
    let mut last_data = Instant::now();

    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let next = match tokio::time::timeout(Duration::from_millis(250), stream.next()).await {
            Err(_) => {
                if last_data.elapsed() > Duration::from_secs(120) {
                    return Err("Google Gemma API stopped sending for 2 minutes.".into());
                }
                flush_pending(&mut on_delta, &mut pending_think, &mut pending_content, &mut last_flush);
                continue;
            }
            Ok(next) => next,
        };
        let Some(chunk) = next else { break };
        last_data = Instant::now();
        let chunk = chunk.map_err(|e| format!("Stream interrupted: {e}"))?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let parsed: StreamChunk = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = parsed.error {
                if !err.message.is_empty() {
                    return Err(err.message);
                }
            }
            for candidate in parsed.candidates {
                let Some(parts) = candidate.content.map(|c| c.parts) else {
                    continue;
                };
                for part in parts {
                    if let Some(call) = part.function_call {
                        if !call.name.trim().is_empty() {
                            tool_calls.push(ToolCall {
                                kind: Some("function".into()),
                                function: ToolFunction {
                                    index: None,
                                    name: call.name,
                                    arguments: crate::ollama::normalize_arguments(call.args),
                                },
                            });
                        }
                    }
                    let text = part.text.unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    if part.thought.unwrap_or(false) {
                        pending_think.push_str(&text);
                        thinking.push_str(&text);
                    } else {
                        pending_content.push_str(&text);
                        content.push_str(&text);
                    }
                    if last_flush.elapsed() > Duration::from_millis(80) {
                        flush_pending(&mut on_delta, &mut pending_think, &mut pending_content, &mut last_flush);
                    }
                }
            }
        }
    }
    flush_pending(&mut on_delta, &mut pending_think, &mut pending_content, &mut last_flush);

    Ok(ModelTurn {
        content,
        thinking,
        tool_calls,
    })
}

fn request_body(model: &str, messages: &[ChatMessage], tools: &Value, think: bool) -> Value {
    let mut system = String::new();
    let mut contents = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&message.content);
            }
            "tool" => contents.push(json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": message.tool_name.clone().unwrap_or_else(|| "tool".into()),
                        "response": { "result": message.content }
                    }
                }]
            })),
            "assistant" => {
                let mut parts = Vec::new();
                if !message.content.trim().is_empty() {
                    parts.push(json!({ "text": message.content }));
                }
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        parts.push(json!({
                            "functionCall": {
                                "name": call.function.name,
                                "args": call.function.arguments
                            }
                        }));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            _ => {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({ "text": message.content }));
                }
                if let Some(images) = &message.images {
                    for image in images.iter().take(4) {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": "image/jpeg",
                                "data": strip_data_url(image)
                            }
                        }));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    let mut generation = json!({ "temperature": 0.3 });
    if supports_thinking(model) {
        generation["thinkingConfig"] = json!({
            "thinkingLevel": if think { "high" } else { "minimal" }
        });
    }
    let mut body = json!({
        "contents": contents,
        "generationConfig": generation
    });
    if !system.trim().is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if let Some(gemini_tools) = gemini_tools(tools) {
        body["tools"] = gemini_tools;
    }
    body
}

fn gemini_tools(tools: &Value) -> Option<Value> {
    let decls: Vec<Value> = tools
        .as_array()?
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let mut parameters = function.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" }));
            uppercase_schema_types(&mut parameters);
            Some(json!({
                "name": function.get("name")?,
                "description": function.get("description").cloned().unwrap_or(Value::String(String::new())),
                "parameters": parameters
            }))
        })
        .collect();
    if decls.is_empty() {
        None
    } else {
        Some(json!([{ "functionDeclarations": decls }]))
    }
}

fn uppercase_schema_types(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(kind)) = map.get_mut("type") {
                kind.make_ascii_uppercase();
            }
            for nested in map.values_mut() {
                uppercase_schema_types(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                uppercase_schema_types(item);
            }
        }
        _ => {}
    }
}

fn flush_pending<F>(
    on_delta: &mut F,
    pending_think: &mut String,
    pending_content: &mut String,
    last_flush: &mut Instant,
) where
    F: FnMut(&str, &str),
{
    if pending_think.is_empty() && pending_content.is_empty() {
        return;
    }
    on_delta(pending_think, pending_content);
    pending_think.clear();
    pending_content.clear();
    *last_flush = Instant::now();
}

fn supports_thinking(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("gemma-4") || model.starts_with("gemini-")
}

fn strip_data_url(image: &str) -> String {
    image
        .split_once(',')
        .map(|(_, data)| data.to_string())
        .unwrap_or_else(|| image.to_string())
}

fn empty_turn() -> ModelTurn {
    ModelTurn {
        content: String::new(),
        thinking: String::new(),
        tool_calls: Vec::new(),
    }
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::ChatMessage;

    #[test]
    fn routes_google_model_ids() {
        assert!(uses_google("gemma-4-31b-it"));
        assert!(!uses_google("gemma4:31b"));
    }

    #[test]
    fn converts_tool_messages() {
        let messages = vec![
            ChatMessage::text("system", "Be careful."),
            ChatMessage::text("user", "Submit the form"),
        ];
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "browser_review",
                "description": "Review fields",
                "parameters": { "type": "object", "properties": {} }
            }
        }]);
        let body = request_body("gemma-4-31b-it", &messages, &tools, true);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be careful.");
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "browser_review");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["type"],
            "OBJECT"
        );
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
        let gemma3 = request_body("gemma-3-27b-it", &messages, &tools, true);
        assert!(gemma3["generationConfig"].get("thinkingConfig").is_none());
    }
}
