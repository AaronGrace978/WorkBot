use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryItem {
    pub id: String,
    /// "playbook" | "pitfall" | "preference" | "skill"
    pub kind: String,
    /// Hostname like "github.com", or "*" for global memories/skills
    pub domain: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub uses: u32,
    #[serde(default = "default_confidence")]
    pub confidence: u8,
    /// "agent" | "reflection" | "user" | "starter"
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub updated_at: u64,
}

fn default_confidence() -> u8 {
    85
}

fn default_source() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    pub total_memories: usize,
    pub playbooks_count: usize,
    pub pitfalls_count: usize,
    pub preferences_count: usize,
    pub skills_count: usize,
    pub domains_learned: usize,
    pub total_applications: u32,
    pub tasks_completed: u32,
    pub self_heals: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySnapshot {
    pub items: Vec<MemoryItem>,
    pub stats: MemoryStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryVault {
    #[serde(default)]
    pub items: Vec<MemoryItem>,
    #[serde(default)]
    pub tasks_completed: u32,
    #[serde(default)]
    pub self_heals: u32,
    #[serde(default)]
    pub seeded: bool,
}

impl Default for MemoryVault {
    fn default() -> Self {
        let mut vault = Self {
            items: Vec::new(),
            tasks_completed: 0,
            self_heals: 0,
            seeded: true,
        };
        vault.seed_starters();
        vault
    }
}

impl MemoryVault {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        let mut vault: Self = serde_json::from_str(&raw).unwrap_or_default();
        if !vault.seeded && vault.items.is_empty() {
            vault.seeded = true;
            vault.seed_starters();
        }
        vault
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, raw).map_err(|e| e.to_string())
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        let mut domains = HashSet::new();
        let mut playbooks_count = 0usize;
        let mut pitfalls_count = 0usize;
        let mut preferences_count = 0usize;
        let mut skills_count = 0usize;
        let mut total_applications = 0u32;

        for item in &self.items {
            if item.domain != "*" && !item.domain.is_empty() {
                domains.insert(item.domain.to_lowercase());
            }
            total_applications = total_applications.saturating_add(item.uses);
            match item.kind.as_str() {
                "playbook" => playbooks_count += 1,
                "pitfall" => pitfalls_count += 1,
                "preference" => preferences_count += 1,
                "skill" => skills_count += 1,
                _ => playbooks_count += 1,
            }
        }

        let mut sorted = self.items.clone();
        sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        MemorySnapshot {
            items: sorted,
            stats: MemoryStats {
                total_memories: self.items.len(),
                playbooks_count,
                pitfalls_count,
                preferences_count,
                skills_count,
                domains_learned: domains.len(),
                total_applications,
                tasks_completed: self.tasks_completed,
                self_heals: self.self_heals,
            },
        }
    }

    pub fn upsert(
        &mut self,
        kind: &str,
        domain: &str,
        title: &str,
        content: &str,
        tags: Vec<String>,
        source: &str,
    ) -> MemoryItem {
        let kind = normalize_kind(kind);
        let domain = normalize_domain(domain);
        let title = clip(title.trim(), 140);
        let content = clip(content.trim(), 2000);
        let now = now_secs();

        let clean_tags: Vec<String> = tags
            .into_iter()
            .map(|t| clip(t.trim().to_lowercase().as_str(), 32))
            .filter(|t| !t.is_empty())
            .take(8)
            .collect();

        if let Some(existing) = self.items.iter_mut().find(|item| {
            item.domain.eq_ignore_ascii_case(&domain)
                && item.kind == kind
                && (item.title.eq_ignore_ascii_case(&title)
                    || similarity_overlap(&item.title, &title) >= 0.8)
        }) {
            existing.title = title;
            existing.content = content;
            if !clean_tags.is_empty() {
                existing.tags = clean_tags;
            }
            existing.confidence = (existing.confidence.saturating_add(5)).min(99);
            existing.updated_at = now;
            return existing.clone();
        }

        let id = format!("mem-{now}-{}", self.items.len() + 1);
        let item = MemoryItem {
            id,
            kind,
            domain,
            title,
            content,
            tags: clean_tags,
            uses: 0,
            confidence: if source == "user" { 98 } else { 85 },
            source: source.trim().to_string(),
            updated_at: now,
        };
        self.items.push(item.clone());
        if self.items.len() > 250 {
            // Keep user items and highest confidence/newest items
            self.items.sort_by(|a, b| {
                let a_user = a.source == "user" || a.kind == "preference" || a.kind == "skill";
                let b_user = b.source == "user" || b.kind == "preference" || b.kind == "skill";
                b_user
                    .cmp(&a_user)
                    .then_with(|| b.uses.cmp(&a.uses))
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
            });
            self.items.truncate(250);
        }
        item
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        self.items.len() != before
    }

    pub fn clear_learned(&mut self) {
        self.items
            .retain(|item| item.source == "user" || item.source == "starter");
    }

    pub fn search(&self, query: &str, domain_filter: Option<&str>) -> Vec<MemoryItem> {
        let q = query.trim().to_lowercase();
        let dom = domain_filter.map(normalize_domain).unwrap_or_default();
        let terms: Vec<&str> = q.split_whitespace().filter(|w| w.len() >= 2).collect();

        let mut scored: Vec<(i32, &MemoryItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let mut score = 0i32;
                if !dom.is_empty() && dom != "*" {
                    if domains_match(&item.domain, &dom) {
                        score += 50;
                    } else if item.domain == "*" {
                        score += 10;
                    }
                }
                if terms.is_empty() {
                    score += 10;
                } else {
                    let hay = format!(
                        "{} {} {} {}",
                        item.title.to_lowercase(),
                        item.content.to_lowercase(),
                        item.domain.to_lowercase(),
                        item.tags.join(" ")
                    );
                    for term in &terms {
                        if hay.contains(term) {
                            score += 25;
                        }
                    }
                }
                if score > 0 {
                    Some((score + (item.confidence as i32 / 10), item))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.updated_at.cmp(&a.1.updated_at)));
        scored.into_iter().take(12).map(|(_, item)| item.clone()).collect()
    }

    /// Selects relevant memories for the current goal and active URL, increments their `uses` count,
    /// and returns a formatted `[memory-vault]` context block plus the list of applied titles.
    pub fn recall_for_context(
        &mut self,
        goal: &str,
        active_url: Option<&str>,
    ) -> (String, Vec<String>) {
        let active_dom = active_url.map(extract_domain).unwrap_or_else(|| "*".into());
        let goal_lower = goal.to_lowercase();
        let goal_terms: Vec<&str> = goal_lower
            .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-')
            .filter(|w| w.len() >= 3)
            .collect();

        let mut selected_indices: Vec<(i32, usize)> = Vec::new();
        for (idx, item) in self.items.iter().enumerate() {
            let mut score = 0i32;
            if item.kind == "preference" {
                score += 60;
            }
            if active_dom != "*" && domains_match(&item.domain, &active_dom) {
                score += 80;
            } else if item.domain != "*" && goal_lower.contains(&item.domain.to_lowercase()) {
                score += 75;
            }
            let hay = format!(
                "{} {} {}",
                item.title.to_lowercase(),
                item.content.to_lowercase(),
                item.tags.join(" ")
            );
            let mut term_hits = 0;
            for term in &goal_terms {
                if hay.contains(term) {
                    term_hits += 1;
                }
            }
            if term_hits > 0 {
                score += (term_hits * 18).min(72);
            }
            // Don't auto-inject starter skills unless their keywords match the goal
            if item.source == "starter" && term_hits < 2 {
                score = 0;
            }
            if score >= 35 {
                selected_indices.push((score + (item.confidence as i32 / 5), idx));
            }
        }

        selected_indices.sort_by(|a, b| b.0.cmp(&a.0));
        selected_indices.truncate(8);

        if selected_indices.is_empty() {
            return (String::new(), Vec::new());
        }

        let mut lines = vec![
            "[memory-vault]".to_string(),
            "Learned site playbooks, pitfalls, and user preferences from previous runs:".to_string(),
        ];
        let mut applied_titles = Vec::new();

        for (_, idx) in selected_indices {
            if let Some(item) = self.items.get_mut(idx) {
                item.uses = item.uses.saturating_add(1);
                applied_titles.push(format!("{} ({})", item.title, item.domain));
                lines.push(format!(
                    "- [{} | domain: {} | confidence: {}%] {}: {}",
                    item.kind.to_uppercase(),
                    item.domain,
                    item.confidence,
                    item.title,
                    item.content
                ));
            }
        }

        (lines.join("\n"), applied_titles)
    }

    pub fn record_run_outcome(
        &mut self,
        goal: &str,
        final_url: Option<&str>,
        summary: &str,
        explicit_lesson: Option<&str>,
        actions_taken: &[String],
        had_recovery: bool,
    ) -> Option<MemoryItem> {
        self.tasks_completed = self.tasks_completed.saturating_add(1);
        if had_recovery {
            self.self_heals = self.self_heals.saturating_add(1);
        }

        let domain = final_url
            .map(extract_domain)
            .filter(|d| d != "*" && d != "blank" && !d.is_empty())
            .unwrap_or_else(|| "*".into());

        if let Some(lesson) = explicit_lesson.map(str::trim).filter(|s| s.len() >= 10) {
            let kind = if had_recovery || lesson.to_lowercase().contains("avoid") || lesson.to_lowercase().contains("instead") {
                "pitfall"
            } else {
                "playbook"
            };
            let title = derive_title(goal, &domain);
            return Some(self.upsert(
                kind,
                &domain,
                &title,
                lesson,
                vec![domain.clone(), "auto-learned".into()],
                "reflection",
            ));
        }

        // If the agent executed a non-trivial browser workflow (>= 3 browser actions) on a real domain,
        // automatically distill the successful action trace into a reusable site playbook!
        if domain != "*" && actions_taken.len() >= 3 {
            let trace: Vec<String> = actions_taken.iter().take(8).cloned().collect();
            let content = format!(
                "Verified workflow for \"{}\": {}. Outcome: {}",
                clip(goal, 100),
                trace.join(" -> "),
                clip(summary, 220)
            );
            let title = derive_title(goal, &domain);
            let kind = if had_recovery { "pitfall" } else { "playbook" };
            return Some(self.upsert(
                kind,
                &domain,
                &title,
                &content,
                vec![domain.clone(), "trajectory".into()],
                "reflection",
            ));
        }

        None
    }

    fn seed_starters(&mut self) {
        let now = now_secs();
        let starters = [
            (
                "skill",
                "*",
                "Deep Page Audit & Table Extract",
                "Scan the full page from top to bottom, extract all structured tables, key metrics, and action items using browser_extract, and produce a clean executive brief.",
                vec!["extract", "table", "audit", "summary"],
            ),
            (
                "skill",
                "*",
                "Form Autopilot with Pre-Submit Verification",
                "Map every field on the current page with browser_fields, fill them all from my instructions in one browser_fill_form call, fix any field that failed, run browser_review to verify zero mismatches, and submit.",
                vec!["form", "review", "submit", "autofill"],
            ),
            (
                "skill",
                "*",
                "Learn & Map This Website",
                "Perform a full-page scan of the current website, map out its primary navigation, search controls, forms, and quirks, and save a reusable domain playbook with memory_save.",
                vec!["learn", "playbook", "map", "memory"],
            ),
            (
                "skill",
                "*",
                "Visual Reference QA Diff",
                "Compare the attached reference image against the live Chrome tab pixel by pixel—checking typography, layout, copy, values, and button states—and report or fix every discrepancy.",
                vec!["visual", "qa", "compare", "image"],
            ),
        ];

        for (idx, (kind, domain, title, content, tags)) in starters.into_iter().enumerate() {
            self.items.push(MemoryItem {
                id: format!("starter-{}", idx + 1),
                kind: kind.into(),
                domain: domain.into(),
                title: title.into(),
                content: content.into(),
                tags: tags.into_iter().map(String::from).collect(),
                uses: 0,
                confidence: 95,
                source: "starter".into(),
                updated_at: now,
            });
        }
    }
}

pub fn extract_domain(url_or_domain: &str) -> String {
    let raw = url_or_domain.trim().to_lowercase();
    if raw.is_empty() || raw == "*" || raw.starts_with("about:") {
        return "*".into();
    }
    let without_scheme = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(&raw);
    let host = without_scheme
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");
    if host.is_empty() {
        "*".into()
    } else {
        host.to_string()
    }
}

fn normalize_domain(domain: &str) -> String {
    extract_domain(domain)
}

fn normalize_kind(kind: &str) -> String {
    match kind.trim().to_lowercase().as_str() {
        "playbook" | "workflow" | "site" => "playbook".into(),
        "pitfall" | "bug" | "error" | "fix" | "quirk" => "pitfall".into(),
        "preference" | "pref" | "user" | "fact" => "preference".into(),
        "skill" | "macro" | "template" => "skill".into(),
        _ => "playbook".into(),
    }
}

fn domains_match(stored: &str, active: &str) -> bool {
    let s = stored.trim().to_lowercase();
    let a = active.trim().to_lowercase();
    if s == "*" || a == "*" {
        return false;
    }
    s == a || a.ends_with(&format!(".{s}")) || s.ends_with(&format!(".{a}"))
}

fn similarity_overlap(a: &str, b: &str) -> f32 {
    let a_words: HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let b_words: HashSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    if a_words.is_empty() || b_words.is_empty() {
        return 0.0;
    }
    let inter = a_words.intersection(&b_words).count() as f32;
    let max_len = a_words.len().max(b_words.len()) as f32;
    inter / max_len
}

fn derive_title(goal: &str, domain: &str) -> String {
    let cleaned = goal
        .lines()
        .next()
        .unwrap_or(goal)
        .trim()
        .trim_end_matches('.');
    let short = clip(cleaned, 68);
    if domain != "*" && !short.to_lowercase().contains(&domain.to_lowercase()) {
        format!("{domain}: {short}")
    } else {
        short
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_matches_domains() {
        assert_eq!(
            extract_domain("https://www.github.com/AaronGrace978/WorkBot"),
            "github.com"
        );
        assert_eq!(extract_domain("about:blank"), "*");
        assert!(domains_match("google.com", "docs.google.com"));
        assert!(!domains_match("google.com", "github.com"));
    }

    #[test]
    fn upserts_and_recalls_domain_playbooks() {
        let mut vault = MemoryVault::default();
        vault.upsert(
            "playbook",
            "https://github.com/pulls",
            "Create Pull Request",
            "Click the green New pull request button [index], then compare branches.",
            vec!["git".into(), "pr".into()],
            "agent",
        );
        vault.upsert(
            "preference",
            "*",
            "Preferred Sign-off",
            "Always format summaries with bullet points.",
            vec!["style".into()],
            "user",
        );

        let (context, applied) =
            vault.recall_for_context("Open a pull request on GitHub", Some("https://github.com"));
        assert!(context.contains("[memory-vault]"));
        assert!(context.contains("Create Pull Request"));
        assert!(context.contains("Preferred Sign-off"));
        assert!(applied.len() >= 2);
    }
}
