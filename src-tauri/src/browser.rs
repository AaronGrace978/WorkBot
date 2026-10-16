use crate::dom;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type PendingCalls = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub index: usize,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatus {
    pub connected: bool,
    pub port: u16,
    pub chrome_running: bool,
    pub debug_open: bool,
    pub tabs: Vec<TabInfo>,
    pub active_title: Option<String>,
    pub active_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub text: String,
    pub jpeg_b64: String,
}

#[derive(Debug, Clone)]
pub struct FileTarget {
    pub index: i64,
    pub kind: String,
    pub label: String,
    pub accept: String,
    pub hidden: bool,
}

#[derive(Debug, Deserialize)]
struct TargetInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws: Option<String>,
}

struct Frame {
    img_w: f64,
    img_h: f64,
    css_w: f64,
    css_h: f64,
    /// The screenshot covers the whole page, so its pixels are page
    /// coordinates rather than viewport coordinates.
    page: bool,
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub index: Option<i64>,
    pub selector: Option<String>,
    pub label: Option<String>,
    pub value: String,
}

struct Target {
    x: f64,
    y: f64,
    label: String,
    blocker: String,
}

fn spec_target(spec: &FieldSpec) -> String {
    if let Some(index) = spec.index {
        format!("[{index}]")
    } else if let Some(selector) = &spec.selector {
        selector.clone()
    } else {
        format!("\"{}\"", spec.label.as_deref().unwrap_or(""))
    }
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(into), Value::Object(from)) = (into.as_object_mut(), from) {
        for (key, value) in from {
            if !value.is_null() {
                into.insert(key, value);
            }
        }
    }
}

fn scroll_key(info: &Value) -> (i64, i64, i64, i64) {
    (
        info["y"].as_i64().unwrap_or(0),
        info["x"].as_i64().unwrap_or(0),
        info["inner"]["top"].as_i64().unwrap_or(0),
        info["inner"]["left"].as_i64().unwrap_or(0),
    )
}

fn scroll_line(info: &Value) -> String {
    let vh = info["vh"].as_f64().unwrap_or(800.0);
    describe_scroll(
        info["y"].as_f64().unwrap_or(0.0),
        info["h"].as_f64().unwrap_or(vh),
        vh,
    )
}

pub fn describe_scroll(y: f64, height: f64, viewport: f64) -> String {
    let below = (height - y - viewport).max(0.0);
    let place = if height <= viewport + 2.0 {
        "the whole page fits on screen".to_string()
    } else if y <= 2.0 {
        format!("top of the page, {below:.0} px more below")
    } else if below <= 2.0 {
        "bottom of the page".to_string()
    } else {
        format!("{y:.0} px above, {below:.0} px below")
    };
    format!(
        "scroll: showing y={y:.0}–{:.0} of {height:.0} px ({place}).",
        (y + viewport).min(height.max(viewport))
    )
}

struct Cdp {
    writer: Arc<Mutex<futures_util::stream::SplitSink<WsStream, Message>>>,
    pending: PendingCalls,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    chooser: Arc<Mutex<Option<Value>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.reader.abort();
    }
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self, String> {
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| format!("Could not attach to Chrome: {e}"))?;
        let (sink, mut incoming) = ws.split();
        let writer = Arc::new(Mutex::new(sink));
        let pending: PendingCalls = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let chooser = Arc::new(Mutex::new(None));
        let writer_task = writer.clone();
        let pending_task = pending.clone();
        let alive_task = alive.clone();
        let chooser_task = chooser.clone();
        let reader = tokio::spawn(async move {
            let mut dialog_id: u64 = 1 << 40;
            while let Some(msg) = incoming.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        let Some(id) = value.get("id").and_then(|v| v.as_u64()) else {
                            if value["method"].as_str() == Some("Page.fileChooserOpened") {
                            *chooser_task.lock().await = Some(value.get("params").cloned().unwrap_or(Value::Null));
                            continue;
                        }
                        if value["method"].as_str() == Some("Page.javascriptDialogOpening") {
                                let reply = json!({
                                    "id": dialog_id,
                                    "method": "Page.handleJavaScriptDialog",
                                    "params": { "accept": true }
                                });
                                dialog_id += 1;
                                let _ = writer_task
                                    .lock()
                                    .await
                                    .send(Message::Text(reply.to_string().into()))
                                    .await;
                            }
                            continue;
                        };
                        let reply = if let Some(err) = value.get("error") {
                            Err(err["message"].as_str().unwrap_or("Chrome rejected the command").to_string())
                        } else {
                            Ok(value.get("result").cloned().unwrap_or(Value::Null))
                        };
                        if let Some(tx) = pending_task.lock().await.remove(&id) {
                            let _ = tx.send(reply);
                        }
                    }
                    Ok(Message::Ping(data)) => {
                        let _ = writer_task.lock().await.send(Message::Pong(data)).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            alive_task.store(false, Ordering::SeqCst);
            let mut waiters = pending_task.lock().await;
            for (_, tx) in waiters.drain() {
                let _ = tx.send(Err("Chrome connection closed".into()));
            }
        });

        let cdp = Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            chooser,
            reader,
        };
        cdp.call("Page.enable", json!({})).await?;
        cdp.call("Runtime.enable", json!({})).await?;
        let _ = cdp
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({
                    "source": "window.alert=function(){};window.confirm=function(){return true};window.prompt=function(){return ''};"
                }),
            )
            .await;
        Ok(cdp)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_for(method, params, Duration::from_secs(8)).await
    }

    async fn call_for(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err("Chrome connection closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let payload = json!({ "id": id, "method": method, "params": params });
        if let Err(err) = self
            .writer
            .lock()
            .await
            .send(Message::Text(payload.to_string().into()))
            .await
        {
            self.pending.lock().await.remove(&id);
            self.alive.store(false, Ordering::SeqCst);
            return Err(format!("Chrome connection failed: {err}"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Chrome connection closed".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                if method.starts_with("Input.") {
                    return Ok(Value::Null);
                }
                Err(format!("Chrome timed out during {method}"))
            }
        }
    }
}

pub struct Browser {
    port: u16,
    conn: Option<Cdp>,
    active_title: Option<String>,
    active_url: Option<String>,
    frame: Option<Frame>,
    http: reqwest::Client,
    wb_next: u64,
}

impl Browser {
    pub fn new(port: u16) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            port,
            conn: None,
            active_title: None,
            active_url: None,
            frame: None,
            http,
            wb_next: 1,
        }
    }

    pub fn set_port(&mut self, port: u16) {
        if port != 0 && port != self.port {
            self.port = port;
            self.conn = None;
        }
    }

    pub fn current_url(&self) -> Option<String> {
        self.active_url.clone()
    }

    pub async fn status(&self) -> BrowserStatus {
        let debug_open = self.port_open().await;
        let tabs = if debug_open {
            self.list_pages().await.unwrap_or_default()
        } else {
            Vec::new()
        };
        let connected = debug_open && self.conn.as_ref().is_some_and(|c| c.alive.load(Ordering::SeqCst));
        BrowserStatus {
            connected,
            port: self.port,
            chrome_running: debug_open,
            debug_open,
            tabs: tabs
                .into_iter()
                .enumerate()
                .map(|(index, tab)| TabInfo {
                    index,
                    title: tab.title,
                    url: tab.url,
                })
                .collect(),
            active_title: self.active_title.clone(),
            active_url: self.active_url.clone(),
        }
    }

    pub async fn launch(&mut self, mode: &str, profile_dir: &Path) -> Result<BrowserStatus, String> {
        match mode {
            "connect" => self.attach_default().await?,
            // Current Chrome releases reject remote debugging against the default
            // profile. All managed modes therefore use Work Bot's own persistent
            // profile. This never closes or interferes with the user's normal Chrome.
            "mine" | "restart" | "separate" | "auto" => {
                if self.port_open().await {
                    self.attach_default().await?;
                } else {
                    std::fs::create_dir_all(profile_dir).map_err(|e| e.to_string())?;
                    self.conn = None;
                    self.spawn_chrome(&separate_profile_args(self.port, profile_dir), None)
                        .await?;
                    self.wait_and_attach().await?;
                }
            }
            _ => return Err("Unknown Chrome launch mode.".into()),
        }
        Ok(self.status().await)
    }

    pub async fn observe(&mut self) -> Result<Observation, String> {
        match self.observe_once().await {
            Ok(obs) => Ok(obs),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(900)).await;
                self.settle().await;
                self.observe_once().await
            }
        }
    }

    pub async fn scan_whole_page(&mut self) -> Result<Observation, String> {
        self.with_connection().await?;
        let _ = self.conn_call("Page.bringToFront", json!({})).await;
        self.settle().await;
        let scan = self.eval_wb(dom::FULL_SCAN_JS).await?;
        let form_map = self.eval_wb(dom::FIELDS_JS).await.ok();
        let has_form = form_map
            .as_ref()
            .and_then(|map| map["fields"].as_array())
            .is_some_and(|fields| !fields.is_empty());
        let viewport_w = scan["vw"].as_f64().unwrap_or(1200.0).max(1.0);
        let viewport_h = scan["vh"].as_f64().unwrap_or(800.0).max(1.0);
        let page_w = scan["width"].as_f64().unwrap_or(viewport_w).clamp(320.0, 2000.0);
        let page_h = scan["height"].as_f64().unwrap_or(viewport_h).max(200.0);
        let (scroll_x, scroll_y) = (
            scan["sx"].as_f64().unwrap_or(0.0),
            scan["sy"].as_f64().unwrap_or(0.0),
        );
        // A whole-page capture briefly resizes the viewport, which makes the page
        // jump. Only do it when the page is short enough to stay legible.
        let whole_page = page_h <= (viewport_h * 2.5).min(4000.0);
        let shot = if whole_page {
            let shot = self
                .conn_call(
                    "Page.captureScreenshot",
                    json!({
                        "format": "jpeg",
                        "quality": 72,
                        "fromSurface": true,
                        "captureBeyondViewport": true,
                        "clip": { "x": 0, "y": 0, "width": page_w, "height": page_h, "scale": 1 }
                    }),
                )
                .await;
            let _ = self
                .eval(&format!(
                    "window.scrollTo({{left:{scroll_x},top:{scroll_y},behavior:'instant'}});true"
                ))
                .await;
            shot?
                .get("data")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or("Chrome did not return a full-page screenshot.")?
        } else {
            self.screenshot().await?
        };
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(shot.trim())
            .unwrap_or_default();
        let (css_w, css_h) = if whole_page { (page_w, page_h) } else { (viewport_w, viewport_h) };
        let (img_w, img_h) = jpeg_size(&bytes).unwrap_or((css_w as u32, css_h as u32));
        self.frame = Some(Frame {
            img_w: img_w as f64,
            img_h: img_h as f64,
            css_w,
            css_h,
            page: whole_page,
        });

        let mut lines = vec![
            format!("FULL PAGE SCAN — {}", scan["title"].as_str().unwrap_or("")),
            format!("url: {}", scan["url"].as_str().unwrap_or("")),
            format!(
                "page: {page_w:.0}x{page_h:.0} css px; viewport {viewport_w:.0}x{viewport_h:.0}, currently showing y={scroll_y:.0}–{:.0}.",
                scroll_y + viewport_h
            ),
            if whole_page {
                format!("screenshot: the whole page ({img_w}x{img_h} px). x,y clicks use these page pixels and scroll automatically.")
            } else {
                format!("screenshot: the current viewport only ({img_w}x{img_h} px). Every control on the page is still listed below.")
            },
            "Indexes stay valid until the page navigates. Clicking, typing, or filling an index scrolls to it automatically; do not scroll just to reach an index.".into(),
        ];
        if let Some(items) = scan["items"].as_array() {
            for item in items {
                // The form map below describes fields in more detail.
                if has_form && item["f"].as_bool() == Some(true) {
                    continue;
                }
                let mut desc = format!(
                    "[{}] {}",
                    item["i"].as_i64().unwrap_or(0),
                    item["tag"].as_str().unwrap_or("el")
                );
                if let Some(text) = item["text"].as_str().filter(|text| !text.is_empty()) {
                    desc.push_str(&format!(" \"{text}\""));
                }
                if let Some(value) = item["value"].as_str().filter(|value| !value.is_empty()) {
                    desc.push_str(&format!(" value=\"{value}\""));
                }
                desc.push_str(&format!(
                    " at page y={:.0}{}",
                    item["y"].as_f64().unwrap_or(0.0),
                    if item["view"].as_bool() == Some(true) { " (in view)" } else { "" }
                ));
                lines.push(desc);
            }
        }
        if let Some(map) = form_map.as_ref().filter(|_| has_form) {
            lines.push(dom::format_form_map(map));
        }
        if let Ok(targets) = self.file_targets().await {
            lines.push(describe_file_targets(&targets));
        }
        if let Some(text) = scan["pageText"].as_str().filter(|text| !text.trim().is_empty()) {
            lines.push(format!("whole-page text:\n{text}"));
        }
        if let Some(url) = scan["url"].as_str() {
            self.active_url = Some(url.to_string());
        }
        if let Some(title) = scan["title"].as_str() {
            self.active_title = Some(title.to_string());
        }
        Ok(Observation {
            text: lines.join("\n"),
            jpeg_b64: shot,
        })
    }

    pub async fn reattach(&mut self) -> Result<(), String> {
        if !self.port_open().await {
            return Err("Chrome debug port is closed.".into());
        }
        self.with_connection().await
    }

    pub async fn read_text(&mut self, max: usize) -> Result<String, String> {
        self.with_connection().await?;
        let text = self
            .eval(&format!(
                "(document.body ? document.body.innerText : '').replace(/\\n{{3,}}/g, '\\n\\n').slice(0, {max})"
            ))
            .await?;
        let text = text.as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            return Ok("The page has no readable text.".into());
        }
        Ok(text)
    }

    pub async fn review_form(&mut self) -> Result<String, String> {
        self.with_connection().await?;
        let map = self.eval_wb(dom::FIELDS_JS).await?;
        Ok(dom::format_review(&map))
    }

    /// The F12-style map of every fillable field on the page.
    pub async fn form_fields(&mut self) -> Result<String, String> {
        self.with_connection().await?;
        let map = self.eval_wb(dom::FIELDS_JS).await?;
        let scroll = &map["scroll"];
        Ok(format!(
            "{}\n(page scroll: y={} of {} px, viewport {} px tall)",
            dom::format_form_map(&map),
            scroll["y"].as_i64().unwrap_or(0),
            scroll["h"].as_i64().unwrap_or(0),
            scroll["vh"].as_i64().unwrap_or(0)
        ))
    }

    pub async fn is_final_action_target(
        &mut self,
        index: Option<i64>,
        x: Option<f64>,
        y: Option<f64>,
        selector: Option<&str>,
    ) -> Result<bool, String> {
        self.with_connection().await?;
        let target = if let Some(index) = index {
            format!("window.__wb.get({index})")
        } else if let Some(selector) = selector {
            let quoted = serde_json::to_string(selector).map_err(|e| e.to_string())?;
            format!("(function(){{try{{return window.__wb.queryAll({quoted})[0]||null;}}catch(e){{return null;}}}})()")
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.css_point(x, y).await?;
            format!("document.elementFromPoint({cx}, {cy})")
        } else {
            return Ok(false);
        };
        let result = self
            .eval_wb(&format!(
                r#"(function(){{
  const el = {target};
  if (!el) return false;
  const type = (el.getAttribute('type') || '').toLowerCase();
  const text = (el.innerText || el.value || el.getAttribute('aria-label') || el.getAttribute('title') || '').replace(/\s+/g,' ').trim().toLowerCase();
  return type === 'submit' || /\b(submit|send|save|confirm|finish|complete|publish|post|place order|pay|purchase|sign up|create account)\b/.test(text);
}})()"#
            ))
            .await?;
        Ok(result.as_bool().unwrap_or(false))
    }

    async fn observe_once(&mut self) -> Result<Observation, String> {
        self.with_connection().await?;
        let _ = self.conn_call("Page.bringToFront", json!({})).await;
        if let Ok(win) = self.conn_call("Browser.getWindowForTarget", json!({})).await {
            if win["bounds"]["windowState"].as_str() == Some("minimized") {
                let _ = self
                    .conn_call(
                        "Browser.setWindowBounds",
                        json!({ "windowId": win["windowId"], "bounds": { "windowState": "normal" } }),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
        let snap = self.eval_wb(dom::SNAPSHOT_JS).await?;
        let shot = match self.screenshot().await {
            Ok(shot) => shot,
            Err(err) => {
                let _ = self.eval(HIDE_OVERLAY_JS).await;
                return Err(err);
            }
        };
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let css_w = snap["vw"].as_f64().unwrap_or(1.0).max(1.0);
        let css_h = snap["vh"].as_f64().unwrap_or(1.0).max(1.0);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(shot.trim())
            .map_err(|e| format!("Could not read the screenshot: {e}"))?;
        let (img_w, img_h) = jpeg_size(&bytes).unwrap_or((css_w as u32, css_h as u32));
        self.frame = Some(Frame {
            img_w: img_w as f64,
            img_h: img_h as f64,
            css_w,
            css_h,
            page: false,
        });
        let scale_x = img_w as f64 / css_w;
        let scale_y = img_h as f64 / css_h;
        let mut lines = Vec::new();
        lines.push(format!(
            "url: {}",
            snap["url"].as_str().unwrap_or("")
        ));
        lines.push(format!("title: {}", snap["title"].as_str().unwrap_or("")));
        lines.push(format!(
            "screenshot: {img_w}x{img_h} pixels. viewport: {css_w:.0}x{css_h:.0} css pixels."
        ));
        lines.push(describe_scroll(
            snap["scroll"]["y"].as_f64().unwrap_or(0.0),
            snap["scroll"]["h"].as_f64().unwrap_or(css_h),
            css_h,
        ));
        lines.push(
            "Number badges in the image match these indexes. Click by index when you can. x,y are screenshot pixels."
                .into(),
        );
        if let Some(items) = snap["items"].as_array() {
            if items.is_empty() {
                lines.push("No clickable elements were found in the viewport. Click by x,y or scroll.".into());
            }
            for item in items {
                let x = item["x"].as_f64().unwrap_or(0.0) * scale_x;
                let y = item["y"].as_f64().unwrap_or(0.0) * scale_y;
                let tag = item["tag"].as_str().unwrap_or("el");
                let kind = item["type"].as_str().unwrap_or("");
                let role = item["role"].as_str().unwrap_or("");
                let text = item["text"].as_str().unwrap_or("");
                let value = item["value"].as_str().unwrap_or("");
                let href = item["href"].as_str().unwrap_or("");
                let mut desc = format!("[{}] {}", item["i"].as_i64().unwrap_or(0), tag);
                if !kind.is_empty() {
                    desc.push_str(&format!(" type={kind}"));
                }
                if !role.is_empty() {
                    desc.push_str(&format!(" role={role}"));
                }
                if !text.is_empty() {
                    desc.push_str(&format!(" \"{text}\""));
                }
                if !value.is_empty() {
                    desc.push_str(&format!(" value=\"{value}\""));
                }
                if !href.is_empty() {
                    desc.push_str(&format!(" href={href}"));
                }
                desc.push_str(&format!(" at ({:.0},{:.0})", x, y));
                lines.push(desc);
            }
        }
        if let Some(off) = snap["off"].as_array().filter(|off| !off.is_empty()) {
            lines.push(
                "Form fields outside the viewport (use their index directly; actions scroll to them):".into(),
            );
            for field in off {
                lines.push(format!(
                    "[{}] {} \"{}\" = \"{}\" ({})",
                    field["i"].as_i64().unwrap_or(0),
                    field["kind"].as_str().unwrap_or("field"),
                    field["label"].as_str().unwrap_or(""),
                    field["value"].as_str().unwrap_or(""),
                    field["dir"].as_str().unwrap_or("off-screen")
                ));
            }
        }
        if let Ok(targets) = self.file_targets().await {
            lines.push(describe_file_targets(&targets));
        }
        if let Some(text) = snap["pageText"].as_str() {
            let text = text.trim();
            if !text.is_empty() {
                lines.push(format!("visible text:\n{text}"));
            }
        }
        if let Some(url) = snap["url"].as_str() {
            self.active_url = Some(url.to_string());
        }
        if let Some(title) = snap["title"].as_str() {
            self.active_title = Some(title.to_string());
        }
        Ok(Observation {
            text: lines.join("\n"),
            jpeg_b64: shot,
        })
    }

    pub async fn click(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>, button: &str, count: u32) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let (cx, cy, label, covered) = if let Some(index) = index {
            let target = self.element_point(index).await?;
            (
                target.x,
                target.y,
                format!("element [{index}] {}", target.label),
                target.blocker,
            )
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.css_point(x, y).await?;
            (cx, cy, format!("point ({x:.0},{y:.0})"), String::new())
        } else {
            return Err("Pass an element index or screenshot x and y.".into());
        };
        let before = self.page_ids().await;
        // Fire exactly one trusted click. Calling el.click() before dispatching
        // mouse events consumed one-time login links twice.
        self.mouse_click(cx, cy, button, count.clamp(1, 3)).await?;
        let mut note = self.follow_new_tab(&before).await;
        if !covered.is_empty() {
            note.push_str(&format!(
                " Warning: {covered} was on top of that spot, so it may have received the click. Close the overlay, or use browser_dom_edit js_click or hide."
            ));
        }
        self.settle().await;
        Ok(format!("Clicked {label} in Chrome.{note}"))
    }

    /// Fill many fields in one call. Fields are set through the DOM the way
    /// DevTools would, then verified; fields the page rejects are retried with
    /// trusted input, and custom dropdowns are opened and picked.
    pub async fn fill_form(&mut self, specs: &[FieldSpec]) -> Result<(String, bool), String> {
        self.with_connection().await?;
        if specs.is_empty() {
            return Err("browser_fill_form needs a fields list of {index or label, value}.".into());
        }
        if specs.len() > 80 {
            return Err("Fill at most 80 fields per call.".into());
        }
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let mut results = Vec::with_capacity(specs.len());
        for spec in specs {
            let mut result = self.fill_one(spec).await;
            // Dependent fields (State after Country, extra questions after a
            // radio) often appear a moment after the previous change.
            let note = result["note"].as_str().unwrap_or("");
            if result["ok"].as_bool() != Some(true)
                && (note.starts_with("not found") || note.starts_with("no option matches"))
            {
                tokio::time::sleep(Duration::from_millis(700)).await;
                result = self.fill_one(spec).await;
            }
            results.push(result);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let all_ok = results.iter().all(|r| r["ok"].as_bool() == Some(true));
        Ok((dom::format_fill_results(&results), all_ok))
    }

    async fn fill_one(&mut self, spec: &FieldSpec) -> Value {
        let target = spec_target(spec);
        let arg = json!([{
            "index": spec.index,
            "selector": spec.selector,
            "label": spec.label,
            "value": spec.value,
        }]);
        let mut result = match self.eval_wb(&dom::with_arg(dom::FILL_JS, &arg)).await {
            Ok(value) => value["results"][0].clone(),
            Err(err) => json!({ "ok": false, "note": err }),
        };
        if !result.is_object() {
            result = json!({ "ok": false, "note": "the page did not answer" });
        }
        if result["target"].as_str().unwrap_or("").is_empty() {
            result["target"] = json!(target);
        }
        let Some(id) = result["i"].as_i64() else {
            return result;
        };
        if result["custom"].as_bool() == Some(true) {
            let picked = self.fill_combobox(id, &spec.value, result["tag"].as_str() == Some("input")).await;
            merge(&mut result, picked);
        } else if result["retype"].as_bool() == Some(true) {
            match self.type_text(Some(id), None, None, &spec.value, true, false).await {
                Ok(_) => {
                    let check = self.verify_field(id, &spec.value).await;
                    merge(&mut result, check);
                    if result["ok"].as_bool() != Some(true) {
                        result["note"] = json!("the page did not keep this value; check its format, or browser_inspect the field");
                    }
                }
                Err(err) => result["note"] = json!(err),
            }
        } else if let Some(trusted) = result["trusted"].as_i64() {
            match self.click(Some(trusted), None, None, "left", 1).await {
                Ok(_) => {
                    let check = self.verify_field(trusted, &spec.value).await;
                    merge(&mut result, check);
                }
                Err(err) => result["note"] = json!(err),
            }
        }
        for key in ["custom", "retype", "trusted", "pick", "kind", "tag"] {
            if let Some(map) = result.as_object_mut() {
                map.remove(key);
            }
        }
        result
    }

    async fn verify_field(&mut self, index: i64, value: &str) -> Value {
        let arg = json!({ "i": index, "value": value });
        self.eval_wb(&dom::with_arg(dom::VERIFY_JS, &arg))
            .await
            .unwrap_or_else(|err| json!({ "ok": false, "note": err }))
    }

    /// Open a custom dropdown or autocomplete, type to filter if it is a text
    /// box, then click the option that matches `value`.
    async fn fill_combobox(&mut self, index: i64, value: &str, typeable: bool) -> Value {
        let opener = match self.element_point(index).await {
            Ok(point) => point,
            Err(err) => return json!({ "ok": false, "note": err }),
        };
        if let Err(err) = self.mouse_click(opener.x, opener.y, "left", 1).await {
            return json!({ "ok": false, "note": err });
        }
        if typeable && !value.trim().is_empty() {
            let clear = format!(
                "(function(){{const el=window.__wb.get({index});if(!el)return false;try{{el.focus({{preventScroll:true}});}}catch(e){{}}window.__wb.nativeSet(el,'');window.__wb.fire(el,'input');return true;}})()"
            );
            let _ = self.eval_wb(&clear).await;
            let _ = self.conn_call("Input.insertText", json!({ "text": value })).await;
        }
        let arg = json!({ "value": value });
        let mut found = Value::Null;
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            found = self
                .eval_wb(&dom::with_arg(dom::OPTION_FIND_JS, &arg))
                .await
                .unwrap_or(Value::Null);
            if found["pick"].as_i64().is_some() {
                break;
            }
        }
        if let Some(pick) = found["pick"].as_i64() {
            match self.element_point(pick).await {
                Ok(point) => {
                    let _ = self.mouse_click(point.x, point.y, "left", 1).await;
                }
                Err(err) => return json!({ "ok": false, "note": err }),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            let mut check = self.verify_field(index, value).await;
            if check["ok"].as_bool() != Some(true) {
                // Many widgets render the chosen option away from the opener,
                // so the opener's text cannot confirm the pick.
                check["ok"] = json!(true);
                check["value"] = found["text"].clone();
                check["note"] = json!("picked this option; confirm it with browser_review");
            }
            return check;
        }
        let check = self.verify_field(index, value).await;
        if check["ok"].as_bool() == Some(true) {
            return check;
        }
        let _ = self.press_key("Escape").await;
        let note = if found["count"].as_u64().unwrap_or(0) == 0 {
            "opened it, but no options appeared; browser_look to see the widget"
        } else {
            "no option matches that value"
        };
        json!({ "ok": false, "note": note, "choices": found["choices"].clone(), "value": check["value"].clone() })
    }

    /// DevTools-style view of the page HTML: the main form by default, or the
    /// subtree at an index or CSS selector.
    pub async fn inspect(&mut self, index: Option<i64>, selector: Option<&str>, max: usize) -> Result<String, String> {
        self.with_connection().await?;
        let arg = json!({ "index": index, "selector": selector, "max": max.clamp(1000, 30000) });
        let view = self.eval_wb(&dom::with_arg(dom::INSPECT_JS, &arg)).await?;
        if let Some(err) = view["error"].as_str() {
            return Err(err.to_string());
        }
        Ok(format!(
            "HTML of {} (like the F12 Elements panel; data-wb=\"N\" is the Work Bot index to use with browser_click, browser_fill_form, or browser_dom_edit):\n{}",
            view["what"].as_str().unwrap_or("the page"),
            view["html"].as_str().unwrap_or("")
        ))
    }

    /// Edit the live DOM the way you would in the F12 Elements panel.
    pub async fn dom_edit(
        &mut self,
        action: &str,
        index: Option<i64>,
        selector: Option<&str>,
        name: Option<&str>,
        value: Option<&str>,
        all: bool,
    ) -> Result<String, String> {
        self.with_connection().await?;
        if index.is_none() && selector.is_none() {
            return Err("browser_dom_edit needs an index or a CSS selector.".into());
        }
        let arg = json!({
            "action": action,
            "index": index,
            "selector": selector,
            "name": name,
            "value": value,
            "all": all,
        });
        let result = self.eval_wb(&dom::with_arg(dom::DOM_EDIT_JS, &arg)).await?;
        if result["ok"].as_bool() != Some(true) {
            return Err(result["error"].as_str().unwrap_or("The edit failed.").to_string());
        }
        let items: Vec<&str> = result["items"]
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if action == "js_click" {
            self.settle().await;
        }
        Ok(format!(
            "Applied {action} to {} element{}: {}",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            items.join("; ")
        ))
    }

    pub async fn type_text(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>, text: &str, clear: bool, submit: bool) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        if text.chars().count() > 8000 {
            return Err("That text is too long to type in one step.".into());
        }
        let (focus, cx, cy) = if let Some(index) = index {
            let target = self.element_point(index).await?;
            (format!("window.__wb.get({index})"), target.x, target.y)
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.css_point(x, y).await?;
            (format!("document.elementFromPoint({cx}, {cy})"), cx, cy)
        } else {
            return Err("Pass the field index, or x and y, plus the text to type.".into());
        };
        let quoted = serde_json::to_string(text).map_err(|e| e.to_string())?;
        let clear_js = if clear { "true" } else { "false" };
        let prep = self
            .eval_wb(&format!(
                "(function(){{const wb=window.__wb;const el={focus};if(!el)return null;try{{el.focus({{preventScroll:true}});}}catch(e){{}}if({clear_js}){{if(el.tagName==='INPUT'||el.tagName==='TEXTAREA'){{wb.nativeSet(el,'');wb.fire(el,'input');}}else if(el.isContentEditable){{try{{el.ownerDocument.getSelection().selectAllChildren(el);el.ownerDocument.execCommand('delete');}}catch(e){{}}}}}}return {{tag:el.tagName}};}})()"
            ))
            .await?;
        if prep.is_null() {
            return Err("Could not find that field. Look at the page again.".into());
        }
        self.mouse_click(cx, cy, "left", 1).await?;
        if clear {
            // The click can drop a caret mid-text; select everything so the
            // inserted text replaces it.
            let _ = self
                .eval_wb(&format!(
                    "(function(){{const el={focus};if(!el)return false;if(typeof el.select==='function')el.select();else if(el.isContentEditable)el.ownerDocument.getSelection().selectAllChildren(el);return true;}})()"
                ))
                .await;
        }
        self.conn_call("Input.insertText", json!({ "text": text })).await?;
        let check = self
            .eval_wb(&format!(
                "(function(){{const wb=window.__wb;const el={focus};if(!el||!('value' in el)||(el.tagName!=='INPUT'&&el.tagName!=='TEXTAREA'))return {{ok:true,mode:'plain'}};const want={quoted};if(String(el.value).includes(want))return {{ok:true,mode:'insert'}};wb.nativeSet(el,want);wb.fire(el,'input');wb.fire(el,'change');return {{ok:String(el.value).includes(want),mode:'setter'}};}})()"
            ))
            .await?;
        if submit {
            self.press_key("Enter").await?;
            self.settle().await;
        }
        let mode = check["mode"].as_str().unwrap_or("typed");
        Ok(format!(
            "Typed {} characters into the field ({mode}).",
            text.chars().count()
        ))
    }

    pub async fn press(&mut self, key: &str) -> Result<String, String> {
        self.with_connection().await?;
        self.press_key(key).await?;
        if key.eq_ignore_ascii_case("enter") {
            self.settle().await;
        }
        Ok(format!("Pressed {key}."))
    }

    pub async fn hover(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let (cx, cy) = self.point(index, x, y).await?;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": cx, "y": cy }),
        )
        .await?;
        Ok("Moved the pointer.".into())
    }

    pub async fn scroll(&mut self, dx: f64, dy: f64, x: Option<f64>, y: Option<f64>, to_index: Option<i64>) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        if let Some(index) = to_index {
            let found = self
                .eval_wb(&format!(
                    "(function(){{const el=window.__wb.get({index});if(!el)return false;window.__wb.reveal(el,true);return true;}})()"
                ))
                .await?;
            if found.as_bool() != Some(true) {
                return Err(format!("Element [{index}] is not on the page. Look again."));
            }
            let info = self.scroll_info(None).await?;
            return Ok(format!(
                "Brought element [{index}] to the middle of the viewport. {}",
                scroll_line(&info)
            ));
        }
        let (cx, cy) = match (x, y) {
            (Some(x), Some(y)) => self.css_point(x, y).await?,
            _ => {
                let size = self
                    .eval("({x: window.innerWidth/2, y: window.innerHeight/2})")
                    .await?;
                (
                    size["x"].as_f64().unwrap_or(400.0),
                    size["y"].as_f64().unwrap_or(300.0),
                )
            }
        };
        let before = self.scroll_info(Some((cx, cy))).await?;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": cx,
                "y": cy,
                "deltaX": dx,
                "deltaY": dy
            }),
        )
        .await?;
        // Smooth-scrolling pages keep moving after the wheel event. Wait until
        // the position stops changing so the next screenshot is not mid-scroll.
        let mut after = before.clone();
        let mut still = 0;
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let now = self.scroll_info(Some((cx, cy))).await?;
            if scroll_key(&now) == scroll_key(&after) {
                still += 1;
                if still >= 2 {
                    break;
                }
            } else {
                still = 0;
            }
            after = now;
        }
        if scroll_key(&after) == scroll_key(&before) {
            return Ok(format!(
                "The page did not move: it is already at the {} or the pointer is over an area that does not scroll. {} Fields with an index never need scrolling.",
                if dy > 0.0 { "bottom" } else { "top" },
                scroll_line(&after)
            ));
        }
        let inner = if after["inner"].is_object() { " an inner scroll area" } else { "" };
        Ok(format!(
            "Scrolled{inner} {} by {:.0} px. {}",
            if dy.abs() >= dx.abs() {
                if dy > 0.0 { "down" } else { "up" }
            } else if dx > 0.0 {
                "right"
            } else {
                "left"
            },
            dy.abs().max(dx.abs()),
            scroll_line(&after)
        ))
    }

    async fn scroll_info(&mut self, at: Option<(f64, f64)>) -> Result<Value, String> {
        let arg = match at {
            Some((x, y)) => json!({ "x": x, "y": y }),
            None => json!({}),
        };
        self.eval(&dom::with_arg(dom::SCROLL_INFO_JS, &arg)).await
    }

    pub async fn navigate(&mut self, url: &str) -> Result<String, String> {
        let url = normalize_url(url)?;
        self.with_connection().await?;
        self.conn_call("Page.navigate", json!({ "url": url })).await?;
        self.wait_for_load().await;
        self.active_url = Some(url.clone());
        Ok(format!("Opened {url}."))
    }

    pub async fn back(&mut self) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval("history.back()").await?;
        self.wait_for_load().await;
        Ok("Went back.".into())
    }

    pub async fn forward(&mut self) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval("history.forward()").await?;
        self.wait_for_load().await;
        Ok("Went forward.".into())
    }

    pub async fn reload(&mut self) -> Result<String, String> {
        self.with_connection().await?;
        self.conn_call("Page.reload", json!({ "ignoreCache": false })).await?;
        self.wait_for_load().await;
        Ok("Reloaded the current tab.".into())
    }

    pub async fn key_combo(&mut self, combo: &str) -> Result<String, String> {
        self.with_connection().await?;
        let parts: Vec<&str> = combo
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("Pass a key combination such as Control+A or Shift+Tab.".into());
        }
        let mut modifiers = 0u32;
        let mut main_key = "Enter";
        for part in &parts {
            match part.to_lowercase().as_str() {
                "alt" | "option" => modifiers |= 1,
                "ctrl" | "control" => modifiers |= 2,
                "meta" | "cmd" | "command" | "win" | "super" => modifiers |= 4,
                "shift" => modifiers |= 8,
                _ => main_key = part,
            }
        }
        let (key, code, vk) = key_info(main_key);
        for kind in ["keyDown", "keyUp"] {
            self.conn_call(
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind,
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": vk,
                    "nativeVirtualKeyCode": vk,
                    "modifiers": modifiers
                }),
            )
            .await?;
        }
        self.settle().await;
        Ok(format!("Pressed shortcut {combo}."))
    }

    pub async fn drag(
        &mut self,
        from_index: Option<i64>,
        from_x: Option<f64>,
        from_y: Option<f64>,
        to_index: Option<i64>,
        to_x: Option<f64>,
        to_y: Option<f64>,
    ) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let (sx, sy) = self.point(from_index, from_x, from_y).await?;
        let (ex, ey) = self.point(to_index, to_x, to_y).await?;
        let _ = self.conn_call("Page.bringToFront", json!({})).await;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": sx, "y": sy }),
        )
        .await?;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": sx, "y": sy, "button": "left", "clickCount": 1 }),
        )
        .await?;
        let steps = 8;
        for step in 1..=steps {
            let t = step as f64 / steps as f64;
            let mx = sx + (ex - sx) * t;
            let my = sy + (ey - sy) * t;
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.conn_call(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": mx, "y": my, "button": "left" }),
            )
            .await?;
        }
        tokio::time::sleep(Duration::from_millis(35)).await;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": ex, "y": ey, "button": "left", "clickCount": 1 }),
        )
        .await?;
        self.settle().await;
        Ok(format!(
            "Dragged from ({sx:.0},{sy:.0}) to ({ex:.0},{ey:.0})."
        ))
    }

    pub async fn extract_structured(
        &mut self,
        kind: &str,
        selector: Option<&str>,
    ) -> Result<String, String> {
        self.with_connection().await?;
        let kind_json = serde_json::to_string(kind.trim()).map_err(|e| e.to_string())?;
        let sel_json = serde_json::to_string(selector.unwrap_or("").trim()).map_err(|e| e.to_string())?;
        let extracted = self
            .eval(&format!(
                r#"(function(){{
  const kind = {kind_json}.toLowerCase() || 'all';
  const sel = {sel_json};
  const root = (sel ? document.querySelector(sel) : document) || document;
  const clean = s => String(s || '').replace(/\s+/g, ' ').trim();
  const out = {{ title: document.title, url: location.href }};
  if (kind === 'all' || kind === 'headings') {{
    out.headings = [...root.querySelectorAll('h1,h2,h3,h4')].slice(0, 40).map(h => ({{
      level: h.tagName.toLowerCase(),
      text: clean(h.innerText).slice(0, 160)
    }})).filter(h => h.text);
  }}
  if (kind === 'all' || kind === 'tables') {{
    out.tables = [...root.querySelectorAll('table')].slice(0, 10).map((t, idx) => {{
      const rows = [...t.querySelectorAll('tr')].slice(0, 40).map(tr =>
        [...tr.querySelectorAll('th,td')].slice(0, 16).map(cell => clean(cell.innerText).slice(0, 140))
      ).filter(r => r.some(Boolean));
      return {{ tableIndex: idx, rows }};
    }}).filter(t => t.rows.length > 0);
  }}
  if (kind === 'all' || kind === 'links') {{
    out.links = [...root.querySelectorAll('a[href]')].slice(0, 50).map(a => ({{
      text: clean(a.innerText || a.getAttribute('aria-label')).slice(0, 100),
      href: a.href
    }})).filter(l => l.text && !l.href.startsWith('javascript:'));
  }}
  if (kind === 'all' || kind === 'lists') {{
    out.lists = [...root.querySelectorAll('ul,ol')].slice(0, 15).map(list =>
      [...list.querySelectorAll(':scope > li')].slice(0, 20).map(li => clean(li.innerText).slice(0, 160)).filter(Boolean)
    ).filter(items => items.length > 0);
  }}
  if (kind === 'selector' && sel) {{
    out.matches = [...document.querySelectorAll(sel)].slice(0, 40).map((el, i) => ({{
      index: i,
      tag: el.tagName.toLowerCase(),
      text: clean(el.innerText || el.value || el.getAttribute('aria-label')).slice(0, 240),
      href: el.href || undefined
    }}));
  }}
  return out;
}})()"#
            ))
            .await?;
        serde_json::to_string_pretty(&extracted).map_err(|e| e.to_string())
    }

    pub async fn close_tab(&mut self, index: Option<usize>) -> Result<String, String> {
        let pages = self.list_pages().await?;
        if pages.is_empty() {
            return Err("No Chrome tabs are open.".into());
        }
        let target = if let Some(idx) = index {
            pages
                .get(idx)
                .ok_or_else(|| format!("There is no tab [{idx}]."))?
        } else {
            let current_url = self.active_url.as_deref().unwrap_or("");
            pages
                .iter()
                .find(|p| p.url == current_url)
                .unwrap_or(&pages[0])
        };
        let endpoint = format!("http://127.0.0.1:{}/json/close/{}", self.port, target.id);
        let _ = self.http.get(endpoint).send().await;
        self.conn = None;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = self.attach_default().await;
        Ok(format!("Closed tab \"{}\".", target.title))
    }

    pub async fn select_option(&mut self, index: i64, label: &str) -> Result<String, String> {
        let spec = FieldSpec {
            index: Some(index),
            selector: None,
            label: None,
            value: label.to_string(),
        };
        let (text, ok) = self.fill_form(std::slice::from_ref(&spec)).await?;
        if ok {
            Ok(text)
        } else {
            Err(text)
        }
    }

    pub async fn tabs_text(&self) -> Result<String, String> {
        let pages = self.list_pages().await?;
        if pages.is_empty() {
            return Ok("No Chrome tabs are open.".into());
        }
        let mut lines = vec!["Open tabs:".to_string()];
        for (index, page) in pages.iter().enumerate() {
            lines.push(format!("[{index}] {} — {}", page.title, page.url));
        }
        Ok(lines.join("\n"))
    }

    pub async fn switch_tab(&mut self, index: usize) -> Result<String, String> {
        let pages = self.list_pages().await?;
        let Some(page) = pages.get(index) else {
            return Err(format!("There is no tab [{index}]."));
        };
        let ws = page
            .ws
            .clone()
            .ok_or("That tab does not expose a debugger socket.")?;
        self.attach_ws(&ws, &page.title, &page.url).await?;
        Ok(format!("Switched to tab [{index}] {}.", page.title))
    }

    pub async fn new_tab(&mut self, url: &str) -> Result<String, String> {
        let url = if url.trim().is_empty() {
            "about:blank".to_string()
        } else {
            normalize_url(url)?
        };
        let endpoint = format!(
            "http://127.0.0.1:{}/json/new?{}",
            self.port,
            encode_query(&url)
        );
        let response = self
            .http
            .put(endpoint)
            .send()
            .await
            .map_err(|e| format!("Could not open a tab: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(format!("Chrome refused to open a tab ({status})."));
        }
        let target: TargetInfo = response.json().await.map_err(|e| e.to_string())?;
        if let Some(ws) = target.ws.clone() {
            self.attach_ws(&ws, &target.title, &target.url).await?;
        } else {
            self.attach_default().await?;
        }
        if url != "about:blank" {
            self.wait_for_load().await;
        }
        Ok(format!("Opened a new tab at {url}."))
    }

    pub async fn file_targets(&mut self) -> Result<Vec<FileTarget>, String> {
        self.with_connection().await?;
        let value = self.eval(FILE_CENSUS_JS).await?;
        let mut targets = Vec::new();
        if let Some(files) = value["files"].as_array() {
            for item in files {
                targets.push(FileTarget {
                    index: item["i"].as_i64().unwrap_or(0),
                    kind: "file".into(),
                    label: item["label"].as_str().unwrap_or("").to_string(),
                    accept: item["accept"].as_str().unwrap_or("").to_string(),
                    hidden: item["hidden"].as_bool().unwrap_or(false),
                });
            }
        }
        if let Some(drops) = value["drops"].as_array() {
            for item in drops {
                targets.push(FileTarget {
                    index: item["i"].as_i64().unwrap_or(0),
                    kind: "drop".into(),
                    label: item["text"].as_str().unwrap_or("").to_string(),
                    accept: String::new(),
                    hidden: false,
                });
            }
        }
        Ok(targets)
    }

    pub async fn upload_file(&mut self, file_index: i64, path: &Path) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.file_targets().await?;
        let object_id = self
            .object_id(&format!(
                "(function(){{const el=window.__wb_files&&window.__wb_files[{file_index}];return el||null;}})()"
            ))
            .await?;
        let _ = self.conn_call("DOM.enable", json!({})).await;
        let node = self
            .conn_call("DOM.requestNode", json!({ "objectId": object_id }))
            .await?;
        let _ = self
            .conn_call("Runtime.releaseObject", json!({ "objectId": object_id }))
            .await;
        let node_id = node["nodeId"].as_u64().ok_or("Chrome did not return that file input.")?;
        let file = chrome_file_arg(path)?;
        self.conn_call(
            "DOM.setFileInputFiles",
            json!({ "nodeId": node_id, "files": [file] }),
        )
        .await?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        Ok(format!("Uploaded {name} into file input [{file_index}]."))
    }

    pub async fn drop_file(
        &mut self,
        drop_index: Option<i64>,
        x: Option<f64>,
        y: Option<f64>,
        path: &Path,
    ) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        let (cx, cy) = if let Some(index) = drop_index {
            let point = self
                .eval_wb(&format!(
                    "(function(){{const el=window.__wb_drops&&window.__wb_drops[{index}];if(!el)return null;return window.__wb.reveal(el);}})()"
                ))
                .await?;
            if point.is_null() {
                return Err(format!("Drop zone [{index}] is not on the page. Look again."));
            }
            (point["x"].as_f64().unwrap_or(0.0), point["y"].as_f64().unwrap_or(0.0))
        } else {
            self.point(None, x, y).await?
        };
        let file = chrome_file_arg(path)?;
        let data = json!({
            "items": [],
            "files": [file],
            "dragOperationsMask": 1
        });
        for kind in ["dragEnter", "dragOver", "drop"] {
            self.conn_call(
                "Input.dispatchDragEvent",
                json!({ "type": kind, "x": cx, "y": cy, "data": data }),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        Ok(format!("Dropped {name} at ({cx:.0},{cy:.0})."))
    }

    pub async fn upload_via_chooser(
        &mut self,
        click_index: Option<i64>,
        x: Option<f64>,
        y: Option<f64>,
        path: &Path,
    ) -> Result<String, String> {
        self.with_connection().await?;
        if let Some(conn) = self.conn.as_ref() {
            *conn.chooser.lock().await = None;
        }
        self.conn_call("Page.setInterceptFileChooserDialog", json!({ "enabled": true }))
            .await?;
        let clicked = self.click(click_index, x, y, "left", 1).await;
        if let Err(err) = clicked {
            let _ = self
                .conn_call("Page.setInterceptFileChooserDialog", json!({ "enabled": false }))
                .await;
            return Err(err);
        }
        let mut opened = false;
        for _ in 0..30 {
            let ready = self
                .conn
                .as_ref()
                .is_some_and(|conn| conn.chooser.try_lock().is_ok_and(|slot| slot.is_some()));
            if ready {
                opened = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let file = chrome_file_arg(path)?;
        let accepted = if opened {
            self.conn_call(
                "Page.handleFileChooser",
                json!({ "action": "accept", "files": [file] }),
            )
            .await
        } else {
            Err("The page did not open a file chooser. Use file_index for a file input or drop_index for a drop zone.".into())
        };
        let _ = self
            .conn_call("Page.setInterceptFileChooserDialog", json!({ "enabled": false }))
            .await;
        accepted?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        Ok(format!("Sent {name} through the file chooser."))
    }

    pub async fn render_html_pdf(&mut self, html: &str) -> Result<Vec<u8>, String> {
        self.with_connection().await?;
        let created = match self
            .conn_call(
                "Target.createTarget",
                json!({ "url": "about:blank", "background": true }),
            )
            .await
        {
            Ok(value) if value["targetId"].as_str().is_some() => value,
            _ => {
                self.conn_call("Target.createTarget", json!({ "url": "about:blank" }))
                    .await?
            }
        };
        let target_id = created["targetId"]
            .as_str()
            .ok_or("Chrome did not open a page for the PDF.")?
            .to_string();
        let mut socket = None;
        for _ in 0..20 {
            if let Ok(pages) = self.list_pages().await {
                if let Some(page) = pages.into_iter().find(|page| page.id == target_id) {
                    socket = page.ws;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let result = if let Some(socket) = socket {
            print_html_pdf(&socket, html).await
        } else {
            Err("The PDF page did not start.".into())
        };
        let _ = self
            .http
            .get(format!("http://127.0.0.1:{}/json/close/{target_id}", self.port))
            .send()
            .await;
        result
    }

    async fn object_id(&mut self, expression: &str) -> Result<String, String> {
        let result = self
            .conn_call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": false }),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err("Could not reach that file input.".into());
        }
        result["result"]["objectId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "That file input is not on the page. Look again.".into())
    }

    async fn point(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>) -> Result<(f64, f64), String> {
        if let Some(index) = index {
            let target = self.element_point(index).await?;
            return Ok((target.x, target.y));
        }
        match (x, y) {
            (Some(x), Some(y)) => self.css_point(x, y).await,
            _ => Err("Pass an element index or screenshot x and y.".into()),
        }
    }

    /// Viewport point for an indexed element. Scrolls instantly, and only when
    /// the element is not already comfortably on screen.
    async fn element_point(&mut self, index: i64) -> Result<Target, String> {
        let point = self
            .eval_wb(&format!(
                r#"(function(){{
  const wb = window.__wb;
  let el = wb.get({index});
  if (!el) return null;
  const field = wb.isField(el);
  const label = field ? wb.label(el) : wb.clean(el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('placeholder') || '', 80);
  // Styled checkboxes and radios hide the real input; click its label instead.
  if (!wb.shown(el) && el.labels) {{ const lab = [...el.labels].find(wb.shown); if (lab) el = lab; }}
  const p = wb.reveal(el);
  return {{ x: p.x, y: p.y, label, blocker: wb.blocker(el, p) }};
}})()"#
            ))
            .await?;
        if point.is_null() {
            return Err(format!(
                "Element [{index}] is not on the page anymore. Call browser_look or browser_fields for current indexes."
            ));
        }
        Ok(Target {
            x: point["x"].as_f64().unwrap_or(0.0),
            y: point["y"].as_f64().unwrap_or(0.0),
            label: point["label"].as_str().unwrap_or("").to_string(),
            blocker: point["blocker"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Convert screenshot pixels to a viewport point. Whole-page screenshots use
    /// page pixels, so the point is scrolled into view first.
    async fn css_point(&mut self, x: f64, y: f64) -> Result<(f64, f64), String> {
        let Some(frame) = &self.frame else {
            return Ok((x, y));
        };
        let sx = if frame.img_w > 0.0 { frame.css_w / frame.img_w } else { 1.0 };
        let sy = if frame.img_h > 0.0 { frame.css_h / frame.img_h } else { 1.0 };
        let (px, py) = (x * sx, y * sy);
        if !frame.page {
            return Ok((px, py));
        }
        let point = self
            .eval(&format!(
                "(function(){{const px={px},py={py};let left=scrollX,top=scrollY;if(py<top+40||py>top+innerHeight-40)top=Math.max(0,py-innerHeight/2);if(px<left||px>left+innerWidth)left=Math.max(0,px-innerWidth/2);if(left!==scrollX||top!==scrollY)window.scrollTo({{left,top,behavior:'instant'}});return {{x:px-scrollX,y:py-scrollY}};}})()"
            ))
            .await?;
        Ok((
            point["x"].as_f64().unwrap_or(px),
            point["y"].as_f64().unwrap_or(py),
        ))
    }

    async fn mouse_click(&mut self, x: f64, y: f64, button: &str, count: u32) -> Result<(), String> {
        let button = match button {
            "right" | "middle" => button,
            _ => "left",
        };
        let _ = self.conn_call("Page.bringToFront", json!({})).await;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y }),
        )
        .await?;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": button, "clickCount": count }),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.conn_call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": x, "y": y, "button": button, "clickCount": count }),
        )
        .await?;
        Ok(())
    }

    async fn press_key(&mut self, key: &str) -> Result<(), String> {
        let (key, code, vk) = key_info(key);
        for kind in ["keyDown", "keyUp"] {
            let mut params = json!({
                "type": kind,
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk
            });
            if kind == "keyDown" && key.chars().count() == 1 {
                params["text"] = json!(key);
            }
            self.conn_call("Input.dispatchKeyEvent", params).await?;
        }
        Ok(())
    }

    async fn wait_for_load(&mut self) {
        self.settle().await;
    }

    pub async fn settle(&mut self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            let state = tokio::time::timeout(Duration::from_secs(3), self.eval("document.readyState"))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(Value::Null);
            if state.as_str() == Some("complete") {
                tokio::time::sleep(Duration::from_millis(250)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn page_ids(&self) -> Vec<String> {
        self.list_pages()
            .await
            .map(|pages| pages.into_iter().map(|p| p.id).collect())
            .unwrap_or_default()
    }

    async fn follow_new_tab(&mut self, before: &[String]) -> String {
        if before.is_empty() {
            return String::new();
        }
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let Ok(pages) = self.list_pages().await else {
                continue;
            };
            let Some(page) = pages
                .into_iter()
                .rev()
                .find(|p| !before.contains(&p.id) && !p.url.starts_with("devtools://"))
            else {
                continue;
            };
            let Some(ws) = page.ws.clone() else {
                continue;
            };
            return match self.attach_ws(&ws, &page.title, &page.url).await {
                Ok(()) => " It opened a new tab, and Work Bot switched to it.".into(),
                Err(_) => String::new(),
            };
        }
        String::new()
    }

    async fn screenshot(&mut self) -> Result<String, String> {
        let result = self
            .conn_call(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": 82, "fromSurface": true }),
            )
            .await?;
        result["data"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "Chrome did not return a screenshot.".into())
    }

    async fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self
            .conn_call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            let desc = result["exceptionDetails"]["exception"]["description"]
                .as_str()
                .or_else(|| result["exceptionDetails"]["text"].as_str())
                .unwrap_or("The page script failed");
            return Err(desc.to_string());
        }
        Ok(result["result"]["value"].clone())
    }

    /// Evaluate a script that uses `window.__wb`, installing it first if this
    /// document does not have it yet.
    async fn eval_wb(&mut self, body: &str) -> Result<Value, String> {
        let value = self.eval(&dom::with_core(self.wb_next, body)).await?;
        if let Some(next) = value.get("next").and_then(Value::as_u64) {
            self.wb_next = self.wb_next.max(next);
        }
        Ok(value)
    }

    async fn conn_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let Some(conn) = self.conn.as_ref() else {
            return Err("Chrome is not attached.".into());
        };
        if !conn.alive.load(Ordering::SeqCst) {
            return Err("Chrome connection closed".into());
        }
        conn.call(method, params).await
    }

    async fn with_connection(&mut self) -> Result<(), String> {
        let alive = self
            .conn
            .as_ref()
            .is_some_and(|c| c.alive.load(Ordering::SeqCst));
        if !alive {
            self.conn = None;
            self.attach_default().await?;
        }
        Ok(())
    }

    async fn wait_and_attach(&mut self) -> Result<(), String> {
        for _ in 0..60 {
            if self.port_open().await {
                // Chrome's endpoint can appear before its first page target.
                if self.attach_default().await.is_ok() {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err("The Work Bot Chrome window did not become ready. Close only the Work Bot Chrome window and try again.".into())
    }

    async fn attach_default(&mut self) -> Result<(), String> {
        let pages = self.list_pages().await?;
        let page = pages
            .into_iter()
            .find(|page| !page.url.starts_with("devtools://") && !page.url.starts_with("chrome-extension://"))
            .ok_or("Chrome is running, but there is no page tab to control.")?;
        let ws = page.ws.clone().ok_or("Chrome did not provide a debugger socket. Restart it from Work Bot.")?;
        self.attach_ws(&ws, &page.title, &page.url).await
    }

    async fn attach_ws(&mut self, ws: &str, title: &str, url: &str) -> Result<(), String> {
        let conn = Cdp::connect(ws).await?;
        self.conn = Some(conn);
        self.active_title = Some(title.to_string());
        self.active_url = Some(url.to_string());
        let _ = self.conn_call("Page.bringToFront", json!({})).await;
        let _ = self.eval("window.alert=function(){};window.confirm=function(){return true};window.prompt=function(){return ''};true").await;
        Ok(())
    }

    async fn list_pages(&self) -> Result<Vec<TargetInfo>, String> {
        let url = format!("http://127.0.0.1:{}/json/list", self.port);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| {
                format!(
                    "Nothing is listening on port {}. Connect Chrome first.",
                    self.port
                )
            })?;
        let targets: Vec<TargetInfo> = response.json().await.map_err(|e| e.to_string())?;
        Ok(targets
            .into_iter()
            .filter(|target| target.kind == "page")
            .collect())
    }

    async fn port_open(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/json/version", self.port);
        self.http.get(url).send().await.is_ok_and(|r| r.status().is_success())
    }

    async fn spawn_chrome(&self, args: &[String], _extra: Option<PathBuf>) -> Result<(), String> {
        let path = chrome_path()?;
        let mut command = std::process::Command::new(path);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hidden(&mut command)
            .spawn()
            .map_err(|e| format!("Could not start Chrome: {e}"))?;
        Ok(())
    }
}

async fn print_html_pdf(ws: &str, html: &str) -> Result<Vec<u8>, String> {
    let cdp = Cdp::connect(ws).await?;
    let tree = cdp.call("Page.getFrameTree", json!({})).await?;
    let frame_id = tree["frameTree"]["frame"]["id"]
        .as_str()
        .ok_or("Chrome did not return a document frame.")?
        .to_string();
    cdp.call(
        "Page.setDocumentContent",
        json!({ "frameId": frame_id, "html": html }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pdf = cdp
        .call_for(
            "Page.printToPDF",
            json!({
                "printBackground": true,
                "paperWidth": 8.5,
                "paperHeight": 11,
                "marginTop": 0.55,
                "marginBottom": 0.55,
                "marginLeft": 0.65,
                "marginRight": 0.65,
                "displayHeaderFooter": false,
                "preferCSSPageSize": true
            }),
            Duration::from_secs(20),
        )
        .await?;
    let data = pdf["data"].as_str().ok_or("Chrome did not return a PDF.")?;
    let cleaned: String = data.chars().filter(|ch| !ch.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| format!("Could not read the PDF: {e}"))?;
    if !bytes.starts_with(b"%PDF") {
        return Err("Chrome returned something that is not a PDF.".into());
    }
    Ok(bytes)
}

fn chrome_file_arg(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err("That document is missing from disk.".into());
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().to_string();
    let text = text.trim_start_matches(r"\\?\").to_string();
    if text.is_empty() {
        return Err("That document path is empty.".into());
    }
    Ok(text)
}

pub fn describe_file_targets(targets: &[FileTarget]) -> String {
    if targets.is_empty() {
        return "file targets: none. If an Upload button is visible, call docs_attach with its click_index.".into();
    }
    let mut lines = vec!["file targets (use these with docs_attach, not the gold click indexes):".into()];
    for target in targets {
        let mut line = format!("[{} {}]", target.kind, target.index);
        if !target.label.is_empty() {
            line.push_str(&format!(" \"{}\"", target.label.replace('"', "'")));
        }
        if !target.accept.is_empty() {
            line.push_str(&format!(" accept={}", target.accept));
        }
        if target.hidden {
            line.push_str(" hidden");
        }
        lines.push(line);
    }
    lines.join("\n")
}

pub fn choose_file_input<'a>(
    targets: &'a [FileTarget],
    kind: &str,
    explicit: Option<i64>,
) -> Result<&'a FileTarget, String> {
    let files: Vec<&FileTarget> = targets.iter().filter(|target| target.kind == "file").collect();
    if let Some(index) = explicit {
        return files
            .into_iter()
            .find(|target| target.index == index)
            .ok_or_else(|| format!("There is no file input [{index}]."));
    }
    if files.len() == 1 {
        return Ok(files[0]);
    }
    if files.is_empty() {
        return Err("This page has no file input.".into());
    }
    let needles: &[&str] = match kind {
        "cover_letter" | "thank_you" => &["cover", "letter"],
        "resume" => &["resume", "cv"],
        _ => &[],
    };
    if let Some(hit) = files.iter().copied().find(|target| {
        let hay = format!("{} {}", target.label, target.accept).to_lowercase();
        needles.iter().any(|needle| !needle.is_empty() && hay.contains(needle))
    }) {
        return Ok(hit);
    }
    Err("Several file inputs are on the page. Pass file_index.".into())
}

fn separate_profile_args(port: u16, dir: &Path) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={port}"),
        "--remote-allow-origins=*".into(),
        format!("--user-data-dir={}", dir.display()),
        "--profile-directory=Default".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--new-window".into(),
        "about:blank".into(),
    ]
}

fn chrome_path() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();

    #[cfg(target_os = "windows")]
    {
        for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(root) = std::env::var(key) {
                candidates
                    .push(PathBuf::from(root).join("Google\\Chrome\\Application\\chrome.exe"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from(
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            ),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ]);
    }

    #[cfg(target_os = "linux")]
    {
        candidates.extend([
            PathBuf::from("/usr/bin/google-chrome"),
            PathBuf::from("/usr/bin/google-chrome-stable"),
            PathBuf::from("/usr/bin/chromium"),
            PathBuf::from("/usr/bin/chromium-browser"),
            PathBuf::from("/snap/bin/chromium"),
        ]);
    }

    candidates
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| "Google Chrome or Chromium was not found. Install it, then try again.".into())
}

fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Missing a url.".into());
    }
    let with_scheme = if url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("about:")
    {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    if with_scheme.starts_with("http://") || with_scheme.starts_with("https://") || with_scheme == "about:blank" {
        Ok(with_scheme)
    } else {
        Err("Only http and https links can be opened.".into())
    }
}

fn encode_query(value: &str) -> String {
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

fn key_info(key: &str) -> (String, String, i64) {
    let name = match key.trim() {
        "space" | "Spacebar" => "Space",
        "esc" | "Esc" => "Escape",
        "return" | "Return" => "Enter",
        "del" => "Delete",
        other => other,
    };
    let (code, vk) = match name {
        "Enter" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowRight" => ("ArrowRight", 39),
        "ArrowDown" => ("ArrowDown", 40),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        "Space" => ("Space", 32),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                return (other.to_string(), format!("Key{}", other.to_uppercase()), ch.to_ascii_uppercase() as i64);
            }
            return (other.to_string(), other.to_string(), ch as i64);
        }
        other => (other, 0),
    };
    (name.to_string(), code.to_string(), vk)
}

pub fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let marker = data[i];
        i += 1;
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 1 >= data.len() {
            break;
        }
        let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if marker == 0xC0 || marker == 0xC1 || marker == 0xC2 {
            if i + 6 >= data.len() {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
            let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            return Some((w, h));
        }
        if len < 2 {
            return None;
        }
        i += len;
    }
    None
}

const FILE_CENSUS_JS: &str = r#"(function(){
  function labelFor(el) {
    var label = '';
    try {
      if (el.id && window.CSS && CSS.escape) {
        var linked = document.querySelector('label[for="' + CSS.escape(el.id) + '"]');
        if (linked) label = linked.innerText || '';
      }
    } catch (e) {}
    if (!label && el.closest) {
      var parent = el.closest('label');
      if (parent) label = parent.innerText || '';
    }
    if (!label) label = el.getAttribute('aria-label') || el.name || '';
    return String(label).replace(/\s+/g, ' ').trim().slice(0, 90);
  }
  function classText(el) {
    var name = el.className;
    if (name && name.baseVal !== undefined) name = name.baseVal;
    return String(name || '') + ' ' + (el.id || '') + ' ' + (el.getAttribute('data-testid') || '');
  }
  function isDrop(el) {
    var tag = el.tagName;
    if (tag === 'INPUT' || tag === 'BUTTON' || tag === 'A' || tag === 'TEXTAREA' || tag === 'SELECT') return false;
    var hint = classText(el).toLowerCase();
    if (/dropzone|drop-zone|file-drop|filedrop|drag-drop|dragdrop/.test(hint)) return true;
    var text = String(el.innerText || '').replace(/\s+/g, ' ').trim().toLowerCase();
    if (!text || text.length > 160) return false;
    return text.indexOf('drag') >= 0 && (text.indexOf('drop') >= 0 || text.indexOf('upload') >= 0 || text.indexOf('file') >= 0);
  }
  var files = [];
  var drops = [];
  var seen = [];
  function seenHas(el) {
    for (var i = 0; i < seen.length; i++) if (seen[i] === el) return true;
    return false;
  }
  function walk(root) {
    if (!root || !root.querySelectorAll || files.length > 12) return;
    var inputs = root.querySelectorAll('input[type="file"]');
    for (var i = 0; i < inputs.length && files.length < 12; i++) {
      if (!seenHas(inputs[i])) {
        seen.push(inputs[i]);
        files.push(inputs[i]);
      }
    }
    var all = root.querySelectorAll('*');
    for (var j = 0; j < all.length; j++) {
      var el = all[j];
      if (drops.length < 6 && isDrop(el) && !seenHas(el)) {
        var rect = el.getBoundingClientRect();
        if (rect.width >= 80 && rect.height >= 28) {
          seen.push(el);
          drops.push(el);
        }
      }
      if (el.shadowRoot) walk(el.shadowRoot);
    }
  }
  function walkFrames(win, depth) {
    if (!win || depth > 3) return;
    try { walk(win.document); } catch (e) {}
    var count = 0;
    try { count = win.frames.length; } catch (e) { count = 0; }
    for (var i = 0; i < count; i++) {
      try { walkFrames(win.frames[i], depth + 1); } catch (e) {}
    }
  }
  walkFrames(window, 0);
  window.__wb_files = files;
  window.__wb_drops = drops;
  return {
    files: files.map(function(el, i) {
      var rect = el.getBoundingClientRect();
      return {
        i: i,
        accept: el.accept || '',
        label: labelFor(el),
        hidden: rect.width < 4 || rect.height < 4
      };
    }),
    drops: drops.map(function(el, i) {
      return { i: i, text: String(el.innerText || el.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim().slice(0, 90) };
    })
  };
})()"#;

const HIDE_OVERLAY_JS: &str = r#"(function(){var n=document.getElementById('wb-overlay');if(n)n.remove();return true;})()"#;

#[cfg(test)]
mod tests {
    use super::jpeg_size;
    use base64::Engine;
    use std::time::Duration;

    #[test]
    fn picks_the_resume_input_when_several_exist() {
        let targets = vec![
            super::FileTarget {
                index: 0,
                kind: "file".into(),
                label: "Cover letter".into(),
                accept: ".pdf".into(),
                hidden: false,
            },
            super::FileTarget {
                index: 1,
                kind: "file".into(),
                label: "Upload resume".into(),
                accept: ".pdf,.docx".into(),
                hidden: true,
            },
        ];
        let chosen = super::choose_file_input(&targets, "resume", None).unwrap();
        assert_eq!(chosen.index, 1);
        assert!(super::describe_file_targets(&targets).contains("[file 1]"));
    }

    #[test]
    fn reads_jpeg_size() {
        let data = [
            0xFFu8, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x0A, 0x00, 0x14, 0x00,
        ];
        assert_eq!(jpeg_size(&data), Some((20, 10)));
    }

    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn drives_a_form_in_chrome() {
        let port = 9477u16;
        let dir = std::env::temp_dir().join("gemma-work-bot-chrome-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let chrome = super::chrome_path().expect("chrome");
        let child = std::process::Command::new(chrome)
            .args([
                format!("--remote-debugging-port={port}"),
                "--remote-allow-origins=*".to_string(),
                format!("--user-data-dir={}", dir.display()),
                "--headless=new".to_string(),
                "--disable-gpu".to_string(),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "about:blank".to_string(),
            ])
            .spawn()
            .unwrap();
        let _guard = Kill(child);
        let mut browser = super::Browser::new(port);
        let mut ready = false;
        for _ in 0..40 {
            if browser.port_open().await {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(ready, "debug port did not open");
        browser.attach_default().await.unwrap();
        let html = r#"<!doctype html><html><body>
<input id="email" placeholder="Email">
<button id="go">Send</button>
<p id="out"></p>
<script>
window.goClicks = 0;
document.getElementById('go').onclick = function() {
  window.goClicks += 1;
  document.getElementById('out').textContent = 'sent:' + document.getElementById('email').value;
};
</script>
</body></html>"#;
        let quoted = serde_json::to_string(html).unwrap();
        browser
            .eval(&format!("document.open();document.write({quoted});document.close();true"))
            .await
            .unwrap();
        let obs = browser.observe().await.unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(obs.jpeg_b64)
            .unwrap();
        assert!(jpeg_size(&bytes).is_some(), "screenshot was not a jpeg");
        assert!(obs.text.contains("input"), "snapshot missed the field: {}", obs.text);
        let marks = browser
            .eval("JSON.stringify([...document.querySelectorAll('[data-wb]')].map(el => ({i:el.getAttribute('data-wb'), tag:el.tagName, ph:el.getAttribute('placeholder')||'', text:(el.innerText||'').trim()})))")
            .await
            .unwrap();
        let marks: Vec<serde_json::Value> =
            serde_json::from_str(marks.as_str().unwrap_or("[]")).unwrap();
        let email = marks
            .iter()
            .find(|item| item["ph"] == "Email")
            .and_then(|item| item["i"].as_str())
            .unwrap()
            .parse::<i64>()
            .unwrap();
        let button = marks
            .iter()
            .find(|item| item["text"] == "Send")
            .and_then(|item| item["i"].as_str())
            .unwrap()
            .parse::<i64>()
            .unwrap();
        browser
            .type_text(Some(email), None, None, "ada@example.com", true, false)
            .await
            .unwrap();
        assert!(
            browser
                .is_final_action_target(Some(button), None, None, None)
                .await
                .unwrap(),
            "Send button was not recognized as a final action"
        );
        let review = browser.review_form().await.unwrap();
        assert!(
            review.contains("ada@example.com"),
            "form review missed the entered value: {review}"
        );
        let full_scan = browser.scan_whole_page().await.unwrap();
        assert!(full_scan.text.contains("FULL PAGE SCAN"));
        assert!(full_scan.text.contains("Send"));
        browser.click(Some(button), None, None, "left", 1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let out = browser.eval("document.getElementById('out').textContent").await.unwrap();
        assert_eq!(out.as_str(), Some("sent:ada@example.com"), "page said {out}");
        let clicks = browser.eval("window.goClicks").await.unwrap();
        assert_eq!(clicks.as_i64(), Some(1), "one action fired multiple clicks");

        browser
            .eval("document.body.innerHTML = '<button id=\"pop\" onclick=\"alert(1);document.title=\\'after\\'\">Pop</button><a id=\"nt\" href=\"#\" onclick=\"window.open(\\'about:blank#childtab\\');return false\">New</a>'; true")
            .await
            .unwrap();
        browser.observe().await.unwrap();
        let pop = browser
            .eval("document.getElementById('pop').getAttribute('data-wb')")
            .await
            .unwrap();
        let pop: i64 = pop.as_str().unwrap().parse().unwrap();
        let started = std::time::Instant::now();
        browser.click(Some(pop), None, None, "left", 1).await.unwrap();
        let title = browser.eval("document.title").await.unwrap();
        assert_eq!(title.as_str(), Some("after"));
        assert!(started.elapsed() < Duration::from_secs(8), "alert stalled the click");

        browser.observe().await.unwrap();
        let link = browser
            .eval("document.getElementById('nt').getAttribute('data-wb')")
            .await
            .unwrap();
        let link: i64 = link.as_str().unwrap().parse().unwrap();
        let result = browser.click(Some(link), None, None, "left", 1).await.unwrap();
        assert!(result.contains("new tab"), "did not follow the new tab: {result}");
        let href = browser.eval("location.href").await.unwrap();
        assert!(
            href.as_str().unwrap_or("").contains("childtab"),
            "stayed on the old tab: {href} / {result}"
        );
    }

    #[test]
    fn describes_scroll_position() {
        let top = super::describe_scroll(0.0, 3000.0, 800.0);
        assert!(top.contains("y=0–800 of 3000"), "{top}");
        assert!(top.contains("top of the page, 2200 px more below"), "{top}");
        assert!(super::describe_scroll(2200.0, 3000.0, 800.0).contains("bottom of the page"));
        assert!(super::describe_scroll(0.0, 600.0, 800.0).contains("fits on screen"));
    }

    async fn index_of(browser: &mut super::Browser, selector: &str) -> i64 {
        let quoted = serde_json::to_string(selector).unwrap();
        let value = browser
            .eval(&format!("document.querySelector({quoted}).getAttribute('data-wb')"))
            .await
            .unwrap();
        value
            .as_str()
            .unwrap_or_else(|| panic!("{selector} has no index"))
            .parse()
            .unwrap()
    }

    fn spec(index: Option<i64>, label: Option<&str>, value: &str) -> super::FieldSpec {
        super::FieldSpec {
            index,
            selector: None,
            label: label.map(str::to_string),
            value: value.to_string(),
        }
    }

    #[tokio::test]
    async fn fills_a_tricky_form_without_scrolling_around() {
        let port = 9478u16;
        let dir = std::env::temp_dir().join("gemma-work-bot-chrome-form-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let chrome = super::chrome_path().expect("chrome");
        let child = std::process::Command::new(chrome)
            .args([
                format!("--remote-debugging-port={port}"),
                "--remote-allow-origins=*".to_string(),
                format!("--user-data-dir={}", dir.display()),
                "--headless=new".to_string(),
                "--disable-gpu".to_string(),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "--window-size=1000,700".to_string(),
                "about:blank".to_string(),
            ])
            .spawn()
            .unwrap();
        let _guard = Kill(child);
        let mut browser = super::Browser::new(port);
        for _ in 0..40 {
            if browser.port_open().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        browser.attach_default().await.unwrap();
        let html = r#"<!doctype html><html><head><style>html{scroll-behavior:smooth}label{display:block;margin-top:6px}</style></head><body>
<form id="f">
<h2>Personal</h2>
<label for="first">First name</label><input id="first" name="first" required>
<label>Email <input id="email" type="email" required></label>
<label for="dob">Date of birth</label><input id="dob" type="date">
<h2>Location</h2>
<label for="country">Country</label>
<select id="country"><option value="">Select…</option><option value="us">United States</option><option value="ca">Canada</option></select>
<label for="state">State</label><select id="state"><option value="">Select…</option></select>
<fieldset><legend>Authorized to work?</legend>
<label><input type="radio" name="auth" value="y">Yes</label>
<label><input type="radio" name="auth" value="n">No</label></fieldset>
<label><input id="agree" type="checkbox" style="opacity:0;position:absolute;width:1px;height:1px">I agree to the terms</label>
<div id="combo" role="combobox" aria-label="Start date" tabindex="0" style="border:1px solid #999;padding:4px;width:200px">Choose…</div>
<ul id="list" role="listbox" style="display:none"><li role="option">Immediately</li><li role="option">In two weeks</li><li role="option">In a month</li></ul>
<label for="code">Referral code</label><input id="code" readonly>
<div style="height:3000px"></div>
<label for="notes">Notes</label><textarea id="notes"></textarea>
<button id="send" type="submit">Submit application</button>
</form>
<script>
window.inputs = 0;
document.getElementById('first').addEventListener('input', () => window.inputs++);
document.getElementById('country').addEventListener('change', (e) => {
  setTimeout(() => {
    document.getElementById('state').innerHTML = '<option value="">Select…</option>' +
      (e.target.value === 'us' ? '<option>California</option><option>Texas</option>' : '<option>Ontario</option>');
  }, 300);
});
const combo = document.getElementById('combo'), list = document.getElementById('list');
combo.addEventListener('click', () => { list.style.display = 'block'; });
list.addEventListener('click', (e) => {
  const o = e.target.closest('[role=option]');
  if (o) { combo.textContent = o.textContent; list.style.display = 'none'; }
});
document.getElementById('f').addEventListener('submit', (e) => e.preventDefault());
</script></body></html>"#;
        let quoted = serde_json::to_string(html).unwrap();
        browser
            .eval(&format!("document.open();document.write({quoted});document.close();true"))
            .await
            .unwrap();

        let map = browser.form_fields().await.unwrap();
        assert!(map.contains("FORM MAP — 10 fields, 2 required still empty"), "{map}");
        assert!(!map.contains("not listed"), "{map}");
        assert!(map.contains("\"First name\" *required"), "{map}");
        assert!(map.contains("— Location —"), "{map}");
        assert!(map.contains("radio \"Authorized to work?\""), "{map}");
        assert!(map.contains("textarea \"Notes\""), "{map}");
        assert!(map.contains("(below the viewport)"), "{map}");
        assert!(map.contains("(final)"), "{map}");

        let first = index_of(&mut browser, "#first").await;
        browser.observe().await.unwrap();
        browser.form_fields().await.unwrap();
        assert_eq!(index_of(&mut browser, "#first").await, first, "indexes were renumbered");

        let email = index_of(&mut browser, "#email").await;
        let dob = index_of(&mut browser, "#dob").await;
        let country = index_of(&mut browser, "#country").await;
        let state = index_of(&mut browser, "#state").await;
        let agree = index_of(&mut browser, "#agree").await;
        let notes = index_of(&mut browser, "#notes").await;
        let (report, ok) = browser
            .fill_form(&[
                spec(Some(first), None, "Ada"),
                spec(Some(email), None, "ada@example.com"),
                spec(Some(dob), None, "03/15/1990"),
                spec(Some(country), None, "United States"),
                spec(Some(state), None, "Texas"),
                spec(None, Some("Authorized to work?"), "Yes"),
                spec(Some(agree), None, "true"),
                spec(Some(notes), None, "Available for an interview any weekday."),
            ])
            .await
            .unwrap();
        assert!(ok, "{report}");
        let scroll = browser.eval("scrollY").await.unwrap();
        assert_eq!(scroll.as_f64(), Some(0.0), "filling scrolled the page: {report}");
        let values = browser
            .eval("JSON.stringify([first.value, window.inputs > 0, dob.value, country.value, state.value, document.querySelector('[name=auth][value=y]').checked, agree.checked, notes.value])")
            .await
            .unwrap();
        assert_eq!(
            values.as_str().unwrap(),
            r#"["Ada",true,"1990-03-15","us","Texas",true,true,"Available for an interview any weekday."]"#,
            "{report}"
        );

        let combo = index_of(&mut browser, "#combo").await;
        let (report, ok) = browser.fill_form(&[spec(Some(combo), None, "two weeks")]).await.unwrap();
        assert!(ok, "{report}");
        let picked = browser.eval("combo.textContent").await.unwrap();
        assert_eq!(picked.as_str(), Some("In two weeks"), "{report}");

        let code = index_of(&mut browser, "#code").await;
        let (report, ok) = browser.fill_form(&[spec(Some(code), None, "FRIEND-42")]).await.unwrap();
        assert!(!ok && report.contains("read-only"), "{report}");
        browser
            .dom_edit("remove_attribute", Some(code), None, Some("readonly"), None, false)
            .await
            .unwrap();
        let (report, ok) = browser.fill_form(&[spec(Some(code), None, "FRIEND-42")]).await.unwrap();
        assert!(ok, "{report}");

        let html = browser.inspect(None, None, 12000).await.unwrap();
        assert!(html.contains("the main form"), "{html}");
        assert!(html.contains("name=\"first\""), "{html}");
        assert!(html.contains(&format!("data-wb=\"{first}\"")), "{html}");

        let review = browser.review_form().await.unwrap();
        assert!(review.contains("Ada") && !review.contains("Required but empty"), "{review}");
        let send = index_of(&mut browser, "#send").await;
        assert!(browser.is_final_action_target(Some(send), None, None, None).await.unwrap());

        let moved = browser.scroll(0.0, 500.0, None, None, None).await.unwrap();
        assert!(moved.contains("Scrolled down"), "{moved}");
        assert!(moved.contains("scroll: showing y=500"), "smooth scroll was not awaited: {moved}");
        let jumped = browser.scroll(0.0, 0.0, None, None, Some(notes)).await.unwrap();
        assert!(jumped.contains("Brought element"), "{jumped}");
    }
}
