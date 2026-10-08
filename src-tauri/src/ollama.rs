use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            thinking: None,
            images: None,
            tool_calls: None,
            tool_name: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub kind: Option<String>,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

const CHAT_URL: &str = "https://ollama.com/api/chat";

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub content: String,
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    message: Option<StreamMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct StreamMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

pub async fn chat<F>(
    ollama_key: &str,
    gemma_key: &str,
    model: &str,
    think: bool,
    messages: &[ChatMessage],
    tools: &Value,
    on_delta: F,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ModelTurn, String>
where
    F: FnMut(&str, &str),
{
    if crate::gemini::uses_google(model) {
        return crate::gemini::chat(gemma_key, model, think, messages, tools, on_delta, cancel)
            .await;
    }
    chat_ollama(ollama_key, model, think, messages, tools, on_delta, cancel).await
}

async fn chat_ollama<F>(
    api_key: &str,
    model: &str,
    think: bool,
    messages: &[ChatMessage],
    tools: &Value,
    mut on_delta: F,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ModelTurn, String>
where
    F: FnMut(&str, &str),
{
    if api_key.is_empty() {
        return Err("Add your Ollama API key in the sidebar.".into());
    }

    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let body = json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": true,
        "think": think,
        "options": { "temperature": 0.3, "num_ctx": 32768 }
    });

    let mut attempt = 0u32;
    let response = loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(ModelTurn { content: String::new(), thinking: String::new(), tool_calls: Vec::new() });
        }
        let sent = tokio::time::timeout(
            Duration::from_secs(90),
            client.post(CHAT_URL).bearer_auth(api_key).json(&body).send(),
        )
        .await;
        let retry_reason = match sent {
            Err(_) => "Ollama Cloud did not answer in 90 seconds".to_string(),
            Ok(Err(e)) => format!("Could not reach Ollama Cloud: {e}"),
            Ok(Ok(resp)) if resp.status().is_success() => break resp,
            Ok(Ok(resp)) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let detail: String = text.chars().take(400).collect();
                let message = format!("Ollama Cloud returned {status}: {detail}");
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
                return Ok(ModelTurn { content: String::new(), thinking: String::new(), tool_calls: Vec::new() });
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
    let mut finished = false;

    while !finished {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let next = match tokio::time::timeout(Duration::from_millis(250), stream.next()).await {
            Err(_) => {
                if last_data.elapsed() > Duration::from_secs(120) {
                    return Err("Ollama Cloud stopped sending for 2 minutes.".into());
                }
                if !pending_think.is_empty() || !pending_content.is_empty() {
                    on_delta(&pending_think, &pending_content);
                    pending_think.clear();
                    pending_content.clear();
                    last_flush = Instant::now();
                }
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
            if line.is_empty() {
                continue;
            }
            let parsed: StreamChunk = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = parsed.error {
                return Err(err);
            }
            if parsed.done {
                finished = true;
            }
            if let Some(message) = parsed.message {
                let think_delta = message.thinking.unwrap_or_default();
                let content_delta = message.content.unwrap_or_default();
                pending_think.push_str(&think_delta);
                pending_content.push_str(&content_delta);
                if last_flush.elapsed() > Duration::from_millis(80) {
                    on_delta(&pending_think, &pending_content);
                    pending_think.clear();
                    pending_content.clear();
                    last_flush = Instant::now();
                }
                thinking.push_str(&think_delta);
                content.push_str(&content_delta);
                if let Some(calls) = message.tool_calls {
                    if parsed.done {
                        if tool_calls.is_empty() {
                            tool_calls = calls;
                        }
                    } else {
                        merge_tool_calls(&mut tool_calls, calls);
                    }
                }
            }
        }
    }

    if !pending_think.is_empty() || !pending_content.is_empty() {
        on_delta(&pending_think, &pending_content);
    }

    tool_calls.retain(|call| !call.function.name.trim().is_empty());
    for call in &mut tool_calls {
        call.function.arguments =
            normalize_arguments(std::mem::take(&mut call.function.arguments));
    }

    Ok(ModelTurn {
        content,
        thinking,
        tool_calls,
    })
}

fn merge_tool_calls(acc: &mut Vec<ToolCall>, incoming: Vec<ToolCall>) {
    for call in incoming {
        if call.function.name.is_empty() && call.function.arguments.is_null() {
            continue;
        }
        if let Some(index) = call.function.index {
            if let Some(existing) = acc.iter_mut().find(|c| c.function.index == Some(index)) {
                if !call.function.name.is_empty() {
                    existing.function.name = call.function.name;
                }
                if !call.function.arguments.is_null() {
                    existing.function.arguments = call.function.arguments;
                }
                continue;
            }
        }
        let duplicate = acc.iter().any(|existing| {
            existing.function.name == call.function.name
                && existing.function.arguments == call.function.arguments
                && !call.function.name.is_empty()
        });
        if !duplicate {
            acc.push(call);
        }
    }
}

pub fn normalize_arguments(value: Value) -> Value {
    match value {
        Value::String(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "value": raw })),
        Value::Null => json!({}),
        other => other,
    }
}

pub fn arg_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| match v {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

pub fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

pub fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

pub fn arg_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(v)) => *v,
        Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "yes"),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_arguments() {
        let value = normalize_arguments(json!("{\"index\":2}"));
        assert_eq!(arg_i64(&value, "index"), Some(2));
    }
}
