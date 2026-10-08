use crate::ollama::{self, ChatMessage};
use crate::settings::Settings;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const STACK_MODEL: &str = "google-stack";
pub const VISION_MODEL: &str = "paligemma2:3b";
pub const DOM_MODEL: &str = "gemma3:4b";
pub const ROUTER_MODEL: &str = "gemma3:1b";
pub const FLASH_MODEL: &str = "gemini-3.5-flash";
pub const ARGON_MODEL: &str = "gemini-4-argon";

pub fn uses_stack(model: &str) -> bool {
    matches!(model.trim(), STACK_MODEL | "pure-google")
}

pub fn orchestrator_model(stuck: bool) -> &'static str {
    if stuck {
        ARGON_MODEL
    } else {
        FLASH_MODEL
    }
}

pub async fn brief(
    settings: &Settings,
    goal: &str,
    page_text: &str,
    screenshot: Option<&str>,
    cancel: &AtomicBool,
    on_status: impl Fn(&str),
) -> String {
    let mut parts = Vec::new();
    on_status("Gemma 3 1B is routing the page state…");
    if let Some(note) = ask_local(
        settings,
        ROUTER_MODEL,
        8_192,
        None,
        &format!(
            "You are WorkBot's fast Gemma 3 1B router. Reply with one compact JSON object only, no markdown.\n\
             Keys: ready (boolean), route (look|act|wait|done), note (short).\n\
             ready means the page is loaded enough to act.\n\nGoal:\n{}\n\nPage:\n{}",
            clip(goal, 1_200),
            clip(page_text, 6_000)
        ),
        cancel,
    )
    .await
    {
        parts.push(format!("router ({ROUTER_MODEL}): {note}"));
    }

    if let Some(image) = screenshot {
        on_status("PaliGemma 2 Mix is mapping the screenshot…");
        if let Some(note) = ask_local(
            settings,
            VISION_MODEL,
            8_192,
            Some(image),
            "You are PaliGemma 2 Mix. OCR this screenshot. List visible text, icons, and click targets with approximate x,y screenshot pixels from the top left. Be precise for form fields, buttons, and labels. Keep it under 40 short lines.",
            cancel,
        )
        .await
        {
            parts.push(format!("vision ({VISION_MODEL}): {note}"));
        }
    }

    if !page_text.trim().is_empty() {
        on_status("Gemma 3 4B is reading the DOM…");
        if let Some(note) = ask_local(
            settings,
            DOM_MODEL,
            128_000,
            None,
            &format!(
                "You are Gemma 3 4B. Summarize this page DOM/text for the browser agent. Keep the user's goal. List important fields, buttons, errors, and the next 1-3 actions. Stay under 900 words.\n\nGoal:\n{}\n\nPage:\n{}",
                clip(goal, 2_000),
                clip(page_text, 80_000)
            ),
            cancel,
        )
        .await
        {
            parts.push(format!("dom ({DOM_MODEL}): {note}"));
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(
            "[google-stack]\nLocal specialists ran beside Gemini. Prefer [browser] indexes. Use vision coordinates only when no index exists.\n{}",
            parts.join("\n\n")
        )
    }
}

async fn ask_local(
    settings: &Settings,
    model: &str,
    num_ctx: u32,
    image: Option<&str>,
    prompt: &str,
    cancel: &AtomicBool,
) -> Option<String> {
    let mut user = ChatMessage::text("user", prompt);
    if let Some(image) = image {
        user.images = Some(vec![image.to_string()]);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(35),
        ollama::chat_local(
            &settings.ollama_host,
            model,
            &[user],
            |_, _| {},
            cancel,
            num_ctx,
        ),
    )
    .await;
    match result {
        Ok(Ok(turn)) => {
            let text = turn.content.trim();
            if text.is_empty() {
                None
            } else {
                Some(clip(text, 2_400))
            }
        }
        _ => None,
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_stack_id() {
        assert!(uses_stack("google-stack"));
        assert!(!uses_stack("gemma-4-31b-it"));
        assert_eq!(orchestrator_model(false), FLASH_MODEL);
        assert_eq!(orchestrator_model(true), ARGON_MODEL);
    }
}
