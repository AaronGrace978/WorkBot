use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub api_key: String,
    #[serde(default)]
    pub gemma_api_key: String,
    #[serde(default = "default_ollama_host")]
    pub ollama_host: String,
    pub model: String,
    pub think: bool,
    pub debug_port: u16,
    pub max_steps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            gemma_api_key: String::new(),
            ollama_host: default_ollama_host(),
            model: "google-stack".into(),
            think: false,
            debug_port: 9222,
            max_steps: 777,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, raw).map_err(|e| e.to_string())
    }

    pub fn normalized(mut self) -> Self {
        self.api_key = self.api_key.trim().to_string();
        self.gemma_api_key = self.gemma_api_key.trim().to_string();
        self.ollama_host = self.ollama_host.trim().trim_end_matches('/').to_string();
        if self.ollama_host.is_empty() {
            self.ollama_host = default_ollama_host();
        }
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            self.model = "google-stack".into();
        }
        if self.model.len() > 80 {
            self.model.truncate(80);
        }
        if self.debug_port == 0 {
            self.debug_port = 9222;
        }
        // Migrate the original slow defaults once. Explicit custom choices are kept.
        if self.model == "gemma4:31b" && self.think && self.max_steps <= 24 {
            self.think = false;
            self.max_steps = 777;
        }
        self.max_steps = 777;
        self
    }
}

fn default_ollama_host() -> String {
    "http://127.0.0.1:11434".into()
}
