use crate::ollama::{self, ChatMessage};
use crate::settings::Settings;
use crate::stack;
use base64::Engine;
use flate2::read::{DeflateDecoder, ZlibDecoder};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCS: usize = 40;
const MAX_TEXT: usize = 80_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApplicantProfile {
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub headline: String,
    #[serde(default)]
    pub links: String,
    #[serde(default)]
    pub summary: String,
}

impl ApplicantProfile {
    fn normalized(mut self) -> Self {
        self.full_name = clip_trim(&self.full_name, 120);
        self.email = clip_trim(&self.email, 200);
        self.phone = clip_trim(&self.phone, 40);
        self.location = clip_trim(&self.location, 120);
        self.headline = clip_trim(&self.headline, 160);
        self.links = clip_trim(&self.links, 400);
        self.summary = clip_trim(&self.summary, 4000);
        self
    }

    fn is_blank(&self) -> bool {
        self.full_name.trim().is_empty()
            && self.email.trim().is_empty()
            && self.phone.trim().is_empty()
            && self.location.trim().is_empty()
            && self.headline.trim().is_empty()
            && self.links.trim().is_empty()
            && self.summary.trim().is_empty()
    }

    fn contact_line(&self) -> String {
        join_parts(&[
            self.email.clone(),
            self.phone.clone(),
            self.location.clone(),
            self.links.clone(),
        ])
    }

    fn one_line(&self) -> String {
        let line = join_parts(&[
            self.full_name.clone(),
            self.email.clone(),
            self.phone.clone(),
            self.location.clone(),
        ]);
        if line.is_empty() {
            "Name not set".into()
        } else {
            line
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeldDocument {
    id: String,
    title: String,
    kind: String,
    source: String,
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    original_file: String,
    #[serde(default)]
    pdf_name: String,
    #[serde(default)]
    docx_name: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    job_title: String,
    #[serde(default)]
    company: String,
    #[serde(default)]
    created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentInfo {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub source: String,
    pub original_name: String,
    pub has_pdf: bool,
    pub has_docx: bool,
    pub has_original: bool,
    pub text: String,
    pub job_title: String,
    pub company: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSnapshot {
    pub profile: ApplicantProfile,
    pub documents: Vec<DocumentInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Store {
    #[serde(default)]
    profile: ApplicantProfile,
    #[serde(default)]
    documents: Vec<HeldDocument>,
}

#[derive(Debug, Clone, Default)]
pub struct DocumentVault {
    profile: ApplicantProfile,
    documents: Vec<HeldDocument>,
}

pub struct ComposedDocument {
    pub title: String,
    pub html: String,
    pub plain: String,
    pub docx: Vec<u8>,
    pub pdf: Vec<u8>,
}

impl DocumentVault {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        let store: Store = serde_json::from_str(&raw).unwrap_or_default();
        Self {
            profile: store.profile.normalized(),
            documents: store.documents,
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let store = Store {
            profile: self.profile.clone(),
            documents: self.documents.clone(),
        };
        let raw = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
        fs::write(path, raw).map_err(|e| e.to_string())
    }

    pub fn snapshot(&self, dir: &Path) -> DocumentSnapshot {
        DocumentSnapshot {
            profile: self.profile.clone(),
            documents: self
                .documents
                .iter()
                .map(|doc| self.info(dir, doc))
                .collect(),
        }
    }

    pub fn set_profile(&mut self, profile: ApplicantProfile) {
        self.profile = profile.normalized();
    }

    pub fn context_block(&self) -> String {
        let mut lines = vec!["[documents]".to_string()];
        if self.profile.is_blank() && self.documents.is_empty() {
            lines.push(
                "The document holder is empty. If a resume or cover letter is required, call task_done and tell the user to add them in the Documents tab. Do not invent a resume, employers, degrees, or dates.".into(),
            );
            return lines.join("\n");
        }
        lines.push(format!("Applicant: {}", self.profile.one_line()));
        if !self.profile.headline.trim().is_empty() {
            lines.push(format!("Headline: {}", clip(&self.profile.headline, 200)));
        }
        if !self.profile.summary.trim().is_empty() {
            lines.push(format!("Summary: {}", clip(&self.profile.summary, 700)));
        }
        if self.documents.is_empty() {
            lines.push("No files stored yet.".into());
        } else {
            lines.push("Files:".into());
            for doc in &self.documents {
                lines.push(format!(
                    "- id={} kind={} source={} title=\"{}\" company=\"{}\" role=\"{}\" text_chars={}",
                    doc.id,
                    doc.kind,
                    doc.source,
                    doc.title.replace('"', "'"),
                    doc.company.replace('"', "'"),
                    doc.job_title.replace('"', "'"),
                    doc.text.chars().count()
                ));
            }
        }
        if let Some(resume) = self.primary_resume() {
            if !resume.text.trim().is_empty() {
                lines.push(format!(
                    "Primary resume excerpt (id={}):\n{}",
                    resume.id,
                    clip(&resume.text, 1800)
                ));
            }
        }
        lines.push(
            "When applying, docs_read a stored resume if you need the full text. If the posting asks for a cover letter, call docs_draft then docs_attach. docs_attach uploads into a file input, drops a file on a drop zone, or answers an Upload button's file chooser. Use file_index, drop_index, or click_index from the file targets list. Never invent employers, titles, dates, degrees, or metrics that are not in the holder.".into(),
        );
        lines.join("\n")
    }

    pub fn read(&self, id: &str) -> Result<String, String> {
        let doc = self.get(id)?;
        if doc.text.trim().is_empty() {
            return Ok(format!(
                "\"{}\" has no extracted text. Ask the user to paste it in Documents, or attach the original file.",
                doc.title
            ));
        }
        let body = clip(&doc.text, 12_000);
        Ok(format!(
            "{} | {} | {} | id={}\n{}",
            doc.title, doc.kind, doc.source, doc.id, body
        ))
    }

    pub fn list_text(&self) -> String {
        if self.documents.is_empty() && self.profile.is_blank() {
            return "Document holder is empty. Ask the user to add a resume and profile in the Documents tab.".into();
        }
        let mut lines = vec![format!("Applicant: {}", self.profile.one_line())];
        if self.documents.is_empty() {
            lines.push("No files stored.".into());
        }
        for doc in &self.documents {
            lines.push(format!(
                "id={} kind={} source={} title=\"{}\" company=\"{}\" role=\"{}\" text_chars={} pdf={} docx={}",
                doc.id,
                doc.kind,
                doc.source,
                doc.title,
                doc.company,
                doc.job_title,
                doc.text.chars().count(),
                !doc.pdf_name.is_empty(),
                !doc.docx_name.is_empty() || doc.original_name.to_lowercase().ends_with(".docx")
            ));
        }
        lines.join("\n")
    }

    pub fn source_material(&self, source_id: Option<&str>) -> String {
        if let Some(id) = source_id.map(str::trim).filter(|id| !id.is_empty()) {
            if let Ok(doc) = self.get(id) {
                return doc.text.clone();
            }
        }
        if let Some(resume) = self.primary_resume() {
            if !resume.text.trim().is_empty() {
                return resume.text.clone();
            }
        }
        self.documents
            .iter()
            .max_by_key(|doc| doc.text.len())
            .map(|doc| doc.text.clone())
            .unwrap_or_default()
    }

    pub fn profile(&self) -> &ApplicantProfile {
        &self.profile
    }

    pub fn has_material(&self, source_id: Option<&str>) -> bool {
        let source = self.source_material(source_id);
        source.trim().chars().count() + self.profile.summary.trim().chars().count() >= 40
    }

    pub fn add_upload(&mut self, dir: &Path, name: &str, kind: &str, bytes: &[u8]) -> Result<String, String> {
        if bytes.is_empty() {
            return Err("That file is empty.".into());
        }
        if bytes.len() > MAX_BYTES {
            return Err("That file is larger than 8 MB.".into());
        }
        if self.documents.len() >= MAX_DOCS {
            return Err("The document holder is full. Delete a file first.".into());
        }
        let ext = extension_of(name);
        if !matches!(ext.as_str(), "pdf" | "docx" | "doc" | "txt" | "md" | "text" | "rtf" | "html" | "htm") {
            return Err("Add a PDF, Word, text, RTF, or HTML document.".into());
        }
        let id = new_id();
        let folder = dir.join(&id);
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let stored = format!("upload.{}", if ext.is_empty() { "bin" } else { ext.as_str() });
        fs::write(folder.join(&stored), bytes).map_err(|e| e.to_string())?;
        let text = clip(&extract_text(&ext, bytes), MAX_TEXT);
        let title = title_from_filename(name);
        let rel = format!("{id}/{stored}");
        let doc = HeldDocument {
            id: id.clone(),
            title,
            kind: normalize_kind(kind),
            source: "uploaded".into(),
            original_name: safe_filename(name),
            original_file: rel.clone(),
            pdf_name: if ext == "pdf" { rel.clone() } else { String::new() },
            docx_name: if ext == "docx" { rel } else { String::new() },
            text,
            job_title: String::new(),
            company: String::new(),
            created_at: now_secs(),
        };
        self.documents.insert(0, doc);
        Ok(id)
    }

    pub fn store_generated(
        &mut self,
        dir: &Path,
        composed: &ComposedDocument,
        kind: &str,
        job_title: &str,
        company: &str,
    ) -> Result<String, String> {
        if self.documents.len() >= MAX_DOCS {
            return Err("The document holder is full. Delete a file first.".into());
        }
        let id = new_id();
        let folder = dir.join(&id);
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        fs::write(folder.join("letter.pdf"), &composed.pdf).map_err(|e| e.to_string())?;
        fs::write(folder.join("letter.docx"), &composed.docx).map_err(|e| e.to_string())?;
        fs::write(folder.join("letter.html"), composed.html.as_bytes()).map_err(|e| e.to_string())?;
        fs::write(folder.join("letter.txt"), composed.plain.as_bytes()).map_err(|e| e.to_string())?;
        let doc = HeldDocument {
            id: id.clone(),
            title: composed.title.clone(),
            kind: normalize_kind(kind),
            source: "generated".into(),
            original_name: format!("{}.pdf", safe_filename(&composed.title)),
            original_file: String::new(),
            pdf_name: format!("{id}/letter.pdf"),
            docx_name: format!("{id}/letter.docx"),
            text: clip(&composed.plain, MAX_TEXT),
            job_title: clip_trim(job_title, 160),
            company: clip_trim(company, 160),
            created_at: now_secs(),
        };
        self.documents.insert(0, doc);
        Ok(id)
    }

    pub fn update_text(&mut self, id: &str, text: &str) -> Result<(), String> {
        let doc = self.get_mut(id)?;
        doc.text = clip(&text.trim().to_string(), MAX_TEXT);
        Ok(())
    }

    pub fn delete(&mut self, dir: &Path, id: &str) -> Result<(), String> {
        if !valid_id(id) {
            return Err("Unknown document.".into());
        }
        let Some(pos) = self.documents.iter().position(|doc| doc.id == id) else {
            return Err("That document is not in the holder.".into());
        };
        self.documents.remove(pos);
        let folder = dir.join(id);
        if folder.is_dir() {
            let _ = fs::remove_dir_all(folder);
        }
        Ok(())
    }

    pub fn select_format(&self, id: &str, requested: Option<&str>, accept: &str) -> Result<String, String> {
        let doc = self.get(id)?;
        let has_pdf = !doc.pdf_name.is_empty();
        let has_docx = !doc.docx_name.is_empty();
        let has_original = !doc.original_file.is_empty();
        let original_ext = extension_of(&doc.original_name);
        let available = |fmt: &str| match fmt {
            "pdf" => has_pdf,
            "docx" => has_docx,
            "original" => has_original || has_pdf,
            _ => false,
        };
        if let Some(raw) = requested.map(str::trim).filter(|value| !value.is_empty()) {
            let fmt = raw.to_lowercase();
            let fmt = match fmt.as_str() {
                "pdf" | "docx" | "original" => fmt,
                "word" | "doc" => "docx".into(),
                other => return Err(format!("Use pdf, docx, or original, not {other}.")),
            };
            if !available(&fmt) {
                return Err(format!("\"{}\" has no {fmt} file.", doc.title));
            }
            return Ok(fmt);
        }
        let accept = accept.to_lowercase();
        let wants_pdf = accept.contains("pdf");
        let wants_docx = accept.contains("docx") || accept.contains("word");
        if wants_docx && !wants_pdf && has_docx {
            return Ok("docx".into());
        }
        if wants_pdf && has_pdf {
            return Ok("pdf".into());
        }
        if has_pdf {
            return Ok("pdf".into());
        }
        if wants_docx && has_docx {
            return Ok("docx".into());
        }
        if has_original {
            if original_ext == "pdf" && has_pdf {
                return Ok("pdf".into());
            }
            return Ok("original".into());
        }
        if has_docx {
            return Ok("docx".into());
        }
        Err(format!("\"{}\" has no file to attach.", doc.title))
    }

    pub fn absolute(&self, dir: &Path, id: &str, format: &str) -> Result<PathBuf, String> {
        let doc = self.get(id)?;
        let rel = match format {
            "pdf" => doc.pdf_name.as_str(),
            "docx" => doc.docx_name.as_str(),
            "original" => {
                if !doc.original_file.is_empty() {
                    doc.original_file.as_str()
                } else {
                    doc.pdf_name.as_str()
                }
            }
            other => return Err(format!("Unknown format {other}.")),
        };
        if rel.is_empty() {
            return Err(format!("\"{}\" has no {format} file.", doc.title));
        }
        if rel.contains("..") {
            return Err("That document path is not valid.".into());
        }
        let path = dir.join(rel);
        if !path.is_file() {
            return Err(format!("\"{}\" is missing from disk. Add it again.", doc.title));
        }
        Ok(path)
    }

    pub fn preview_path(&self, dir: &Path, id: &str) -> Result<PathBuf, String> {
        let doc = self.get(id)?;
        for format in ["pdf", "docx", "original"] {
            if let Ok(path) = self.absolute(dir, id, format) {
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
        Err(format!("Nothing to open for \"{}\".", doc.title))
    }

    pub fn kind_of(&self, id: &str) -> Result<String, String> {
        Ok(self.get(id)?.kind.clone())
    }

    fn info(&self, dir: &Path, doc: &HeldDocument) -> DocumentInfo {
        DocumentInfo {
            id: doc.id.clone(),
            title: doc.title.clone(),
            kind: doc.kind.clone(),
            source: doc.source.clone(),
            original_name: doc.original_name.clone(),
            has_pdf: self.absolute(dir, &doc.id, "pdf").is_ok(),
            has_docx: self.absolute(dir, &doc.id, "docx").is_ok(),
            has_original: self.absolute(dir, &doc.id, "original").is_ok(),
            text: clip(&doc.text, 50_000),
            job_title: doc.job_title.clone(),
            company: doc.company.clone(),
            created_at: doc.created_at,
        }
    }

    fn get(&self, id: &str) -> Result<&HeldDocument, String> {
        self.documents
            .iter()
            .find(|doc| doc.id == id)
            .ok_or_else(|| "That document is not in the holder. Call docs_list.".into())
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut HeldDocument, String> {
        self.documents
            .iter_mut()
            .find(|doc| doc.id == id)
            .ok_or_else(|| "That document is not in the holder. Call docs_list.".into())
    }

    fn primary_resume(&self) -> Option<&HeldDocument> {
        self.documents
            .iter()
            .filter(|doc| doc.kind == "resume" && !doc.text.trim().is_empty())
            .max_by_key(|doc| doc.text.len())
            .or_else(|| {
                self.documents
                    .iter()
                    .filter(|doc| doc.kind == "resume")
                    .next()
            })
    }
}

pub fn decode_upload(data_base64: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = data_base64.chars().filter(|ch| !ch.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|_| "Could not read that file.".to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("That file is larger than 8 MB.".into());
    }
    Ok(bytes)
}

pub fn compose_document(
    profile: &ApplicantProfile,
    kind: &str,
    body: &str,
    job_title: &str,
    company: &str,
) -> ComposedDocument {
    let kind = normalize_kind(kind);
    let body = body.trim().to_string();
    let date = today_string();
    let title = document_title(&kind, &profile.full_name, job_title, company);
    let contact = profile.contact_line();
    let role_line = join_parts(&[job_title.to_string(), company.to_string()]);
    let plain = plain_text(&profile.full_name, &contact, &date, &role_line, &body);
    let html = letter_html(&profile.full_name, &profile.headline, &contact, &date, &role_line, &kind, &body);
    let docx = docx_bytes(&profile.full_name, &profile.headline, &contact, &date, &role_line, &kind, &body);
    let pdf = fallback_pdf(&profile.full_name, &contact, &date, &role_line, &body);
    ComposedDocument {
        title,
        html,
        plain,
        docx,
        pdf,
    }
}

pub async fn write_body(
    settings: &Settings,
    profile: &ApplicantProfile,
    source: &str,
    kind: &str,
    job_title: &str,
    company: &str,
    job_text: &str,
    notes: &str,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let kind = normalize_kind(kind);
    let model = if stack::uses_stack(&settings.model) {
        stack::select_orchestrator(&settings.model, false).to_string()
    } else {
        settings.model.clone()
    };
    let messages = vec![
        ChatMessage::text("system", DRAFT_RULES),
        ChatMessage::text(
            "user",
            draft_request(profile, source, &kind, job_title, company, job_text, notes),
        ),
    ];
    let turn = ollama::chat(
        &settings.api_key,
        &settings.gemma_api_key,
        &model,
        settings.think,
        &messages,
        &serde_json::Value::Null,
        |_thinking, _content| {},
        cancel,
    )
    .await?;
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("Drafting was stopped.".into());
    }
    let body = clean_draft(&turn.content);
    let min = if kind == "thank_you" { 40 } else { 80 };
    if body.chars().count() < min {
        return Err("The draft was too short to save. Try again with the job description visible.".into());
    }
    Ok(body)
}

const DRAFT_RULES: &str = r#"You write application documents for one real person. Use ONLY facts in the applicant profile and source documents. Never invent employers, job titles, dates, schools, degrees, certifications, promotions, or metrics. If a detail is missing, leave it out. Do not claim the person did something the source does not support. Write in a natural professional voice that is specific to the job, not generic filler. Output only the document text. No markdown fences, no commentary, no letterhead, and no date line."#;

fn draft_request(
    profile: &ApplicantProfile,
    source: &str,
    kind: &str,
    job_title: &str,
    company: &str,
    job_text: &str,
    notes: &str,
) -> String {
    let instruction = match kind {
        "cover_letter" => "Write a cover letter of 250 to 400 words. Open with a salutation. Use specific overlap between the job and the source. Close with the applicant's real name when you know it.",
        "thank_you" => "Write a thank-you note of 120 to 180 words. Refer only to the application context and source facts. Close with the applicant's real name when you know it.",
        "resume" => "Rewrite the source into a clean resume. Put these section labels in ALL CAPS on their own lines when you have facts for them: SUMMARY, EXPERIENCE, EDUCATION, SKILLS. Do not add roles, employers, or dates that are not in the source. If the source is short, keep the resume short.",
        _ => "Write the application document described in the notes. Stay consistent with the source. Do not fabricate credentials, employment, or education.",
    };
    format!(
        "{instruction}\n\nApplicant name: {}\nEmail: {}\nPhone: {}\nLocation: {}\nHeadline: {}\nLinks: {}\nProfile summary:\n{}\n\nRole: {}\nCompany: {}\nNotes: {}\n\nJob posting:\n{}\n\nSource documents:\n{}",
        empty_dash(&profile.full_name),
        empty_dash(&profile.email),
        empty_dash(&profile.phone),
        empty_dash(&profile.location),
        empty_dash(&profile.headline),
        empty_dash(&profile.links),
        empty_dash(&profile.summary),
        empty_dash(job_title),
        empty_dash(company),
        empty_dash(notes),
        clip(job_text, 8000),
        clip(source, 14000)
    )
}

fn clean_draft(text: &str) -> String {
    let mut text = text.trim();
    if let Some(rest) = text.strip_prefix("```") {
        let rest = rest.trim_start_matches(|ch: char| ch.is_ascii_alphanumeric() || ch == '-');
        text = rest.trim().trim_end_matches("```").trim();
    }
    text.to_string()
}

fn letter_html(
    name: &str,
    headline: &str,
    contact: &str,
    date: &str,
    role_line: &str,
    kind: &str,
    body: &str,
) -> String {
    let name = html_escape(&if name.trim().is_empty() { "Applicant" } else { name });
    let headline = html_escape(headline);
    let contact = html_escape(contact);
    let date = html_escape(date);
    let role_line = html_escape(role_line);
    let body_html = if kind == "resume" {
        resume_blocks(body)
    } else {
        paragraphs(body)
    };
    let headline_html = if headline.is_empty() {
        String::new()
    } else {
        format!("<p class=\"headline\">{headline}</p>")
    };
    let role_html = if role_line.is_empty() {
        String::new()
    } else {
        format!("<p class=\"role\">{role_line}</p>")
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>{name}</title>
<style>
  @page {{ size: letter; margin: 0.7in; }}
  body {{ margin: 0; color: #1c1c1c; }}
  h1 {{ font-family: Calibri, "Segoe UI", sans-serif; font-size: 22pt; font-weight: 650; margin: 0; letter-spacing: -0.02em; }}
  .headline, .contact, .date, .role, h2 {{ font-family: Calibri, "Segoe UI", sans-serif; }}
  .headline {{ margin: 2px 0 0; font-size: 11pt; color: #333; }}
  .contact {{ margin: 4px 0 0; font-size: 10.5pt; color: #444; }}
  hr {{ border: 0; border-top: 1.5px solid #222; margin: 12px 0 16px; }}
  .date, .role {{ margin: 0 0 8px; font-size: 11pt; }}
  p {{ font-family: Georgia, "Times New Roman", serif; font-size: 11.5pt; line-height: 1.45; margin: 0 0 12px; }}
  h2 {{ font-size: 12pt; letter-spacing: 0.06em; font-weight: 700; border-bottom: 1px solid #ccc; margin: 16px 0 8px; padding-bottom: 2px; }}
</style></head><body>
<header><h1>{name}</h1>{headline_html}<p class="contact">{contact}</p></header>
<hr>
<p class="date">{date}</p>
{role_html}
{body_html}
</body></html>"#
    )
}

fn paragraphs(body: &str) -> String {
    let mut html = String::new();
    for para in body.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let encoded = html_escape(para).replace('\n', "<br>");
        html.push_str(&format!("<p>{encoded}</p>"));
    }
    html
}

fn resume_blocks(body: &str) -> String {
    let mut html = String::new();
    for para in body.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let lines: Vec<&str> = para.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
        if lines.len() == 1 && is_section_heading(lines[0]) {
            html.push_str(&format!("<h2>{}</h2>", html_escape(lines[0])));
            continue;
        }
        let mut chunk = String::new();
        for line in lines {
            if is_section_heading(line) {
                if !chunk.is_empty() {
                    html.push_str(&format!("<p>{}</p>", chunk.trim_end_matches("<br>")));
                    chunk.clear();
                }
                html.push_str(&format!("<h2>{}</h2>", html_escape(line)));
            } else {
                chunk.push_str(&html_escape(line));
                chunk.push_str("<br>");
            }
        }
        if !chunk.is_empty() {
            html.push_str(&format!("<p>{}</p>", chunk.trim_end_matches("<br>")));
        }
    }
    html
}

fn docx_bytes(
    name: &str,
    headline: &str,
    contact: &str,
    date: &str,
    role_line: &str,
    kind: &str,
    body: &str,
) -> Vec<u8> {
    let mut body_xml = String::new();
    let display = if name.trim().is_empty() { "Applicant" } else { name };
    body_xml.push_str(&w_p(display, true, 36));
    if !headline.trim().is_empty() {
        body_xml.push_str(&w_p(headline.trim(), false, 22));
    }
    if !contact.trim().is_empty() {
        body_xml.push_str(&w_p(contact, false, 20));
    }
    body_xml.push_str("<w:p/>");
    body_xml.push_str(&w_p(date, false, 22));
    if !role_line.trim().is_empty() {
        body_xml.push_str(&w_p(role_line, false, 22));
    }
    body_xml.push_str("<w:p/>");
    for para in body.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        for line in para.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let heading = kind == "resume" && is_section_heading(line);
            body_xml.push_str(&w_p(line, heading, if heading { 24 } else { 22 }));
        }
        body_xml.push_str("<w:p/>");
    }
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body_xml}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1008" w:right="1080" w:bottom="1008" w:left="1080"/></w:sectPr></w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(&mut cursor);
    let opts = || SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let _ = zip.start_file("[Content_Types].xml", opts());
    let _ = zip.write_all(content_types.as_bytes());
    let _ = zip.start_file("_rels/.rels", opts());
    let _ = zip.write_all(rels.as_bytes());
    let _ = zip.start_file("word/document.xml", opts());
    let _ = zip.write_all(document.as_bytes());
    let _ = zip.finish();
    cursor.into_inner()
}

fn w_p(text: &str, bold: bool, size: u32) -> String {
    let bold = if bold { "<w:b/>" } else { "" };
    format!(
        "<w:p><w:r><w:rPr>{bold}<w:sz w:val=\"{size}\"/><w:szCs w:val=\"{size}\"/><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\"/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
        xml_escape(text)
    )
}

struct DrawLine {
    text: String,
    size: f32,
    bold: bool,
    leading: f32,
}

fn fallback_pdf(name: &str, contact: &str, date: &str, role_line: &str, body: &str) -> Vec<u8> {
    let mut lines = Vec::new();
    let display = if name.trim().is_empty() {
        "Applicant".into()
    } else {
        name.trim().to_string()
    };
    lines.push(DrawLine { text: display, size: 18.0, bold: true, leading: 24.0 });
    if !contact.trim().is_empty() {
        for line in wrap_text(contact, 90) {
            lines.push(DrawLine { text: line, size: 10.0, bold: false, leading: 14.0 });
        }
    }
    lines.push(DrawLine { text: String::new(), size: 11.0, bold: false, leading: 12.0 });
    lines.push(DrawLine { text: date.to_string(), size: 11.0, bold: false, leading: 16.0 });
    if !role_line.trim().is_empty() {
        lines.push(DrawLine { text: role_line.to_string(), size: 11.0, bold: false, leading: 16.0 });
    }
    lines.push(DrawLine { text: String::new(), size: 11.0, bold: false, leading: 10.0 });
    for para in body.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        for line in wrap_text(para, 85) {
            lines.push(DrawLine { text: line, size: 11.0, bold: false, leading: 15.0 });
        }
        lines.push(DrawLine { text: String::new(), size: 11.0, bold: false, leading: 8.0 });
    }
    let pages = paginate(lines);
    build_pdf(&pages)
}

fn paginate(lines: Vec<DrawLine>) -> Vec<Vec<DrawLine>> {
    let mut pages = Vec::new();
    let mut page = Vec::new();
    let mut y = 740.0f32;
    for line in lines {
        if y - line.leading < 64.0 && !page.is_empty() {
            pages.push(std::mem::take(&mut page));
            y = 740.0;
        }
        y -= line.leading;
        page.push(line);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

fn build_pdf(pages: &[Vec<DrawLine>]) -> Vec<u8> {
    let font_regular = 3usize;
    let font_bold = 4usize;
    let mut objects: Vec<(usize, Vec<u8>)> = Vec::new();
    objects.push((
        font_regular,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ));
    objects.push((
        font_bold,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_vec(),
    ));
    let mut kids = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        let page_id = 5 + index * 2;
        let content_id = page_id + 1;
        kids.push(format!("{page_id} 0 R"));
        let stream = page_stream(page);
        let content = format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            stream.len(),
            stream
        );
        objects.push((content_id, content.into_bytes()));
        let page_obj = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {content_id} 0 R /Resources << /Font << /F1 {font_regular} 0 R /F2 {font_bold} 0 R >> >> >>"
        );
        objects.push((page_id, page_obj.into_bytes()));
    }
    let kids = kids.join(" ");
    objects.push((
        2,
        format!("<< /Type /Pages /Count {} /Kids [{kids}] >>", pages.len()).into_bytes(),
    ));
    objects.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
    objects.sort_by_key(|(id, _)| *id);

    let mut buf = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = vec![0u64; objects.len() + 2];
    for (id, data) in &objects {
        offsets[*id] = buf.len() as u64;
        let header = format!("{id} 0 obj\n");
        buf.extend_from_slice(header.as_bytes());
        buf.extend_from_slice(data);
        buf.extend_from_slice(b"\nendobj\n");
    }
    let xref = buf.len();
    let size = objects.len() + 1;
    buf.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for id in 1..=objects.len() {
        buf.extend_from_slice(format!("{:010} 00000 n \n", offsets[id]).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").as_bytes());
    buf
}

fn page_stream(page: &[DrawLine]) -> String {
    let mut ops = String::from("BT\n");
    let mut first = true;
    for line in page {
        if first {
            ops.push_str("72 740 Td\n");
            first = false;
        } else {
            ops.push_str(&format!("0 -{:.1} Td\n", line.leading));
        }
        let font = if line.bold { "F2" } else { "F1" };
        ops.push_str(&format!("/{font} {:.0} Tf\n({}) Tj\n", line.size, pdf_escape(&line.text)));
    }
    ops.push_str("ET");
    ops
}

fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim();
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw.split_whitespace() {
            let word = pdf_plain(word);
            if word.len() > max_chars {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                let mut rest = word.as_str();
                while rest.len() > max_chars {
                    lines.push(rest[..max_chars].to_string());
                    rest = &rest[max_chars..];
                }
                current = rest.to_string();
                continue;
            }
            if current.is_empty() {
                current = word;
            } else if current.len() + 1 + word.len() > max_chars {
                lines.push(std::mem::take(&mut current));
                current = word;
            } else {
                current.push(' ');
                current.push_str(&word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

fn pdf_plain(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '—' | '–' | '−' => '-',
            '’' | '‘' | 'ʼ' => '\'',
            '“' | '”' => '"',
            '•' | '·' | '●' => '-',
            ' ' | '\u{2009}' | '\u{202f}' => ' ',
            c if c.is_ascii() => c,
            _ => '?',
        })
        .collect()
}

fn pdf_escape(text: &str) -> String {
    let mut out = String::new();
    for b in pdf_plain(text).bytes() {
        match b {
            b'\\' | b'(' | b')' => {
                out.push('\\');
                out.push(b as char);
            }
            32..=126 => out.push(b as char),
            _ => out.push('?'),
        }
    }
    out
}

pub fn extract_pdf_text(data: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i + 6 < data.len() {
        if is_token(data, i, b"stream") {
            let dict_from = i.saturating_sub(800);
            let dict = &data[dict_from..i];
            let mut start = i + 6;
            if data.get(start) == Some(&b'\r') {
                start += 1;
            }
            if data.get(start) == Some(&b'\n') {
                start += 1;
            }
            if let Some(rel) = find_token(&data[start..], b"endstream") {
                let mut stream = &data[start..start + rel];
                if stream.last() == Some(&b'\n') {
                    stream = &stream[..stream.len() - 1];
                    if stream.last() == Some(&b'\r') {
                        stream = &stream[..stream.len() - 1];
                    }
                }
                let skip = dict.windows(6).any(|w| w == b"/Image") || stream.len() > 2_000_000;
                if !skip {
                    let decoded = if dict.windows(11).any(|w| w == b"FlateDecode") {
                        inflate_pdf(stream).unwrap_or_default()
                    } else {
                        stream.to_vec()
                    };
                    let text = strings_from_content(&decoded);
                    if text.chars().filter(|ch| ch.is_alphabetic()).count() > 8 {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&text);
                    }
                }
                i = start + rel + 9;
                continue;
            }
        }
        i += 1;
    }
    collapse_ws(&out)
}

fn strings_from_content(data: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < data.len() {
        if data[i] == b'(' {
            if let Some((text, next)) = parse_pdf_literal(data, i) {
                if !text.trim().is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text.trim());
                }
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn parse_pdf_literal(data: &[u8], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    let mut raw = Vec::new();
    let mut depth = 1i32;
    while i < data.len() {
        let b = data[i];
        if b == b'\\' && i + 1 < data.len() {
            let n = data[i + 1];
            match n {
                b'n' => raw.push(b'\n'),
                b'r' => raw.push(b'\r'),
                b't' => raw.push(b'\t'),
                b'(' | b')' | b'\\' => raw.push(n),
                b'0'..=b'7' => {
                    let mut val = (n - b'0') as u32;
                    let mut consumed = 1usize;
                    while consumed < 3
                        && i + 1 + consumed < data.len()
                        && (b'0'..=b'7').contains(&data[i + 1 + consumed])
                    {
                        val = val * 8 + (data[i + 1 + consumed] - b'0') as u32;
                        consumed += 1;
                    }
                    raw.push((val & 0xFF) as u8);
                    i += 1 + consumed;
                    continue;
                }
                _ => raw.push(n),
            }
            i += 2;
            continue;
        }
        if b == b'(' {
            depth += 1;
            raw.push(b);
            i += 1;
            continue;
        }
        if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some((String::from_utf8_lossy(&raw).into_owned(), i + 1));
            }
            raw.push(b);
            i += 1;
            continue;
        }
        raw.push(b);
        i += 1;
    }
    None
}

fn inflate_pdf(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut zlib = ZlibDecoder::new(data);
    if zlib.read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    out.clear();
    let mut raw = DeflateDecoder::new(data);
    if raw.read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    None
}

fn is_token(data: &[u8], i: usize, token: &[u8]) -> bool {
    if i + token.len() > data.len() || &data[i..i + token.len()] != token {
        return false;
    }
    let before = i == 0 || data[i - 1].is_ascii_whitespace();
    let after_i = i + token.len();
    let after = after_i >= data.len() || data[after_i].is_ascii_whitespace();
    before && after
}

fn find_token(data: &[u8], token: &[u8]) -> Option<usize> {
    data.windows(token.len()).position(|window| window == token).and_then(|pos| {
        if is_token(data, pos, token) {
            Some(pos)
        } else {
            None
        }
    })
}

fn extract_text(ext: &str, bytes: &[u8]) -> String {
    let text = match ext {
        "txt" | "md" | "text" | "csv" => String::from_utf8_lossy(bytes).into_owned(),
        "html" | "htm" => strip_tags(&String::from_utf8_lossy(bytes)),
        "docx" => extract_docx(bytes).unwrap_or_default(),
        "pdf" => extract_pdf_text(bytes),
        "rtf" => extract_rtf(bytes),
        _ => String::new(),
    };
    collapse_ws(&text)
}

fn extract_docx(bytes: &[u8]) -> Option<String> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut file = archive.by_name("word/document.xml").ok()?;
    let mut xml = String::new();
    file.read_to_string(&mut xml).ok()?;
    Some(docx_xml_text(&xml))
}

fn docx_xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while let Some(rel) = xml[i..].find('<') {
        out.push_str(&decode_xml(&xml[i..i + rel]));
        let rest = &xml[i + rel..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[1..end];
        let closing = tag.starts_with('/');
        let name = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("");
        if closing && name == "w:p" {
            out.push('\n');
        } else if name == "w:tab" {
            out.push('\t');
        } else if name == "w:br" {
            out.push('\n');
        }
        i = i + rel + end + 1;
    }
    collapse_ws(&out)
}

fn extract_rtf(bytes: &[u8]) -> String {
    let chars: Vec<char> = String::from_utf8_lossy(bytes).chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                let c = chars[i];
                if c == '\\' || c == '{' || c == '}' {
                    out.push(c);
                    i += 1;
                } else if c == '\'' && i + 2 < chars.len() {
                    i += 3;
                } else if c.is_ascii_alphabetic() {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    if word == "par" || word == "line" {
                        out.push('\n');
                    }
                    if i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            '{' | '}' => i += 1,
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    collapse_ws(&out)
}

fn strip_tags(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    collapse_ws(&out)
}

fn decode_xml(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn plain_text(name: &str, contact: &str, date: &str, role_line: &str, body: &str) -> String {
    let mut lines = Vec::new();
    if !name.trim().is_empty() {
        lines.push(name.trim().to_string());
    }
    if !contact.trim().is_empty() {
        lines.push(contact.trim().to_string());
    }
    lines.push(String::new());
    lines.push(date.to_string());
    if !role_line.trim().is_empty() {
        lines.push(role_line.trim().to_string());
    }
    lines.push(String::new());
    lines.push(body.trim().to_string());
    lines.join("\n")
}

fn document_title(kind: &str, name: &str, job_title: &str, company: &str) -> String {
    let who = if name.trim().is_empty() { "Applicant" } else { name.trim() };
    let target = if !company.trim().is_empty() {
        company.trim()
    } else {
        job_title.trim()
    };
    match kind {
        "cover_letter" if !target.is_empty() => format!("Cover letter — {target}"),
        "cover_letter" => format!("Cover letter — {who}"),
        "thank_you" if !target.is_empty() => format!("Thank-you note — {target}"),
        "thank_you" => format!("Thank-you note — {who}"),
        "resume" => format!("Resume — {who}"),
        _ if !target.is_empty() => format!("Application note — {target}"),
        _ => format!("Application note — {who}"),
    }
}

fn normalize_kind(kind: &str) -> String {
    match kind.trim().to_lowercase().replace('-', "_").as_str() {
        "resume" | "cv" => "resume",
        "cover" | "cover_letter" | "letter" => "cover_letter",
        "thank_you" | "thanks" | "thankyou" => "thank_you",
        "portfolio" => "portfolio",
        "writing_sample" | "writing" => "writing_sample",
        _ => "other",
    }
    .to_string()
}

pub fn normalize_draft_kind(kind: &str) -> String {
    match normalize_kind(kind).as_str() {
        "cover_letter" | "thank_you" | "resume" => normalize_kind(kind),
        _ => "other".into(),
    }
}

fn is_section_heading(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && t.len() <= 40
        && !t.ends_with('.')
        && t.chars().any(|ch| ch.is_ascii_alphabetic())
        && t.chars().all(|ch| {
            ch.is_ascii_uppercase() || ch.is_ascii_whitespace() || matches!(ch, '&' | '/' | '-' | ',')
        })
}

fn join_parts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn title_from_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let title = base.replace(['_', '-'], " ");
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        "Document".into()
    } else {
        clip(&title, 80)
    }
}

fn safe_filename(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ') {
            out.push(ch);
        }
    }
    let out = out.trim().trim_matches('.').to_string();
    let out: String = out.chars().take(80).collect();
    if out.is_empty() {
        "document".into()
    } else {
        out
    }
}

fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("doc_{n:x}")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 80
        && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

fn clip_trim(text: &str, max: usize) -> String {
    clip(text.trim(), max)
}

fn empty_dash(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        "—".into()
    } else {
        text.to_string()
    }
}

fn collapse_ws(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if lines.last().is_some_and(|last: &String| last.is_empty()) {
                continue;
            }
            lines.push(String::new());
        } else {
            lines.push(line);
        }
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn today_string() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    let (y, m, d) = civil_from_days(days);
    let months = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let name = months.get((m.saturating_sub(1)) as usize).copied().unwrap_or("January");
    format!("{name} {d}, {y}")
}

fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
    }

    #[test]
    fn draft_rules_forbid_invented_facts() {
        assert!(DRAFT_RULES.contains("Never invent employers"));
    }

    #[test]
    fn composed_letter_is_pdf_and_docx() {
        let profile = ApplicantProfile {
            full_name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
            ..ApplicantProfile::default()
        };
        let body = "Dear Hiring Manager,\n\nI am applying for the analyst role. I built notes for the analytical engine and can explain them clearly.\n\nSincerely,\nAda Lovelace";
        let doc = compose_document(&profile, "cover_letter", body, "Analyst", "Analytical Engines");
        assert!(doc.pdf.starts_with(b"%PDF"));
        assert!(doc.html.contains("Ada Lovelace"));
        assert!(doc.plain.contains("analytical engine"));
        let mut archive = ZipArchive::new(std::io::Cursor::new(doc.docx)).unwrap();
        let mut xml = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        assert!(xml.contains("Ada Lovelace"));
        let extracted = extract_pdf_text(&doc.pdf);
        assert!(
            extracted.contains("Ada") || extracted.contains("Hiring"),
            "pdf text was {extracted}"
        );
    }

    #[test]
    fn holder_roundtrip_extracts_text() {
        let dir = std::env::temp_dir().join(format!("wb-docs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let index = dir.join("documents.json");
        let files = dir.join("files");
        let mut vault = DocumentVault::default();
        vault.set_profile(ApplicantProfile {
            full_name: "Ada Lovelace".into(),
            summary: "Mathematician who wrote extensive notes on the analytical engine.".into(),
            ..ApplicantProfile::default()
        });
        vault
            .add_upload(
                &files,
                "Ada Resume.txt",
                "resume",
                b"Experience: wrote notes on the analytical engine.",
            )
            .unwrap();
        vault.save(&index).unwrap();
        let loaded = DocumentVault::load(&index);
        let block = loaded.context_block();
        assert!(block.contains("Ada Lovelace"));
        assert!(block.contains("analytical engine"));
        assert!(loaded.has_material(None));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn docx_roundtrip_text() {
        let profile = ApplicantProfile {
            full_name: "Grace Hopper".into(),
            ..ApplicantProfile::default()
        };
        let doc = compose_document(
            &profile,
            "resume",
            "SUMMARY\n\nBuilt compilers and taught COBOL.\n\nEXPERIENCE\n\nNavy — programming languages.",
            "",
            "",
        );
        let text = extract_docx(&doc.docx).unwrap();
        assert!(text.contains("Grace Hopper"));
        assert!(text.contains("compilers"));
    }
}
