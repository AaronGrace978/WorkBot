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
}

struct Cdp {
    writer: Arc<Mutex<futures_util::stream::SplitSink<WsStream, Message>>>,
    pending: PendingCalls,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
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
        let writer_task = writer.clone();
        let pending_task = pending.clone();
        let alive_task = alive.clone();
        let reader = tokio::spawn(async move {
            let mut dialog_id: u64 = 1 << 40;
            while let Some(msg) = incoming.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        let Some(id) = value.get("id").and_then(|v| v.as_u64()) else {
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
        match tokio::time::timeout(Duration::from_secs(8), rx).await {
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
        }
    }

    pub fn set_port(&mut self, port: u16) {
        if port != 0 && port != self.port {
            self.port = port;
            self.conn = None;
        }
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
        let scan = self.eval(FULL_SCAN_JS).await?;
        let metrics = self
            .conn_call("Page.getLayoutMetrics", json!({}))
            .await
            .unwrap_or(Value::Null);
        let css_width = metrics["cssContentSize"]["width"]
            .as_f64()
            .or_else(|| scan["width"].as_f64())
            .unwrap_or(1200.0)
            .clamp(320.0, 2000.0);
        let actual_height = metrics["cssContentSize"]["height"]
            .as_f64()
            .or_else(|| scan["height"].as_f64())
            .unwrap_or(800.0)
            .max(200.0);
        let capture_height = actual_height.min(8000.0);
        let shot = self
            .conn_call(
                "Page.captureScreenshot",
                json!({
                    "format": "jpeg",
                    "quality": 72,
                    "fromSurface": true,
                    "captureBeyondViewport": true,
                    "clip": {
                        "x": 0,
                        "y": 0,
                        "width": css_width,
                        "height": capture_height,
                        "scale": 1
                    }
                }),
            )
            .await?
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or("Chrome did not return a full-page screenshot.")?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        // Full-page pixels are not viewport coordinates. Prefer page-wide indexes.
        self.frame = None;

        let mut lines = vec![
            format!("FULL PAGE SCAN — {}", scan["title"].as_str().unwrap_or("")),
            format!("url: {}", scan["url"].as_str().unwrap_or("")),
            format!(
                "page size: {:.0}x{actual_height:.0} css pixels; screenshot covers the first {:.0} pixels",
                css_width, capture_height
            ),
            "These indexes cover controls across the whole page. Use an index; call browser_look for a fresh viewport before coordinate clicks.".into(),
        ];
        if actual_height > capture_height {
            lines.push(format!(
                "The page is very long; text and controls below screenshot pixel {:.0} are still listed.",
                capture_height
            ));
        }
        if let Some(items) = scan["items"].as_array() {
            for item in items {
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
                    " at page y={:.0}",
                    item["y"].as_f64().unwrap_or(0.0)
                ));
                lines.push(desc);
            }
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
        let review = self
            .eval(
                r#"(function(){
  const controls = [...document.querySelectorAll('input,textarea,select,[contenteditable="true"]')];
  const fields = controls.map((el, i) => {
    const labels = el.labels ? [...el.labels].map(x => x.innerText.trim()).filter(Boolean) : [];
    const label = labels.join(' / ') || el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('name') || el.id || `field ${i + 1}`;
    const type = (el.getAttribute('type') || el.tagName).toLowerCase();
    let value = type === 'password' ? (el.value ? '••••••' : '') : ('value' in el ? String(el.value || '') : String(el.innerText || ''));
    if (type === 'checkbox' || type === 'radio') value = el.checked ? 'checked' : 'not checked';
    if (el.tagName === 'SELECT') {
      const option = el.options[el.selectedIndex];
      value = option ? `${option.text} (${option.value})` : '';
    }
    return { label: label.replace(/\s+/g,' ').slice(0,120), type, value: value.replace(/\s+/g,' ').slice(0,500) };
  });
  const actions = [...document.querySelectorAll('button,input[type="submit"],input[type="button"],[role="button"]')]
    .map(el => (el.innerText || el.value || el.getAttribute('aria-label') || '').replace(/\s+/g,' ').trim())
    .filter(Boolean).slice(0,30);
  return { title: document.title, url: location.href, fields, finalActions: actions };
})()"#,
            )
            .await?;
        serde_json::to_string_pretty(&review).map_err(|e| e.to_string())
    }

    pub async fn is_final_action_target(
        &mut self,
        index: Option<i64>,
        x: Option<f64>,
        y: Option<f64>,
    ) -> Result<bool, String> {
        self.with_connection().await?;
        let target = if let Some(index) = index {
            format!("document.querySelector('[data-wb=\"{index}\"]')")
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.to_css(x, y);
            format!("document.elementFromPoint({cx}, {cy})")
        } else {
            return Ok(false);
        };
        let result = self
            .eval(&format!(
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
        let snap = self.eval(SNAPSHOT_JS).await?;
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
        let (cx, cy, label) = if let Some(index) = index {
            let point = self
                .eval(&format!(
                    "(function(){{const el=document.querySelector('[data-wb=\"{index}\"]');if(!el)return null;el.scrollIntoView({{block:'center',inline:'nearest'}});const r=el.getBoundingClientRect();el.focus();return {{x:r.left+r.width/2,y:r.top+r.height/2,label:(el.getAttribute('aria-label')||el.innerText||el.getAttribute('placeholder')||'').replace(/\\s+/g,' ').trim().slice(0,80)}};}})()"
                ))
                .await?;
            if point.is_null() {
                return Err(format!("Element [{index}] is not on the page. Look again."));
            }
            (
                point["x"].as_f64().unwrap_or(0.0),
                point["y"].as_f64().unwrap_or(0.0),
                format!("element [{index}] {}", point["label"].as_str().unwrap_or("")),
            )
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.to_css(x, y);
            (cx, cy, format!("point ({x:.0},{y:.0})"))
        } else {
            return Err("Pass an element index or screenshot x and y.".into());
        };
        let before = self.page_ids().await;
        // Fire exactly one trusted click. Calling el.click() before dispatching
        // mouse events consumed one-time login links twice.
        self.mouse_click(cx, cy, button, count.clamp(1, 3)).await?;
        let note = self.follow_new_tab(&before).await;
        self.settle().await;
        Ok(format!("Clicked {label} in Chrome.{note}"))
    }

    pub async fn type_text(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>, text: &str, clear: bool, submit: bool) -> Result<String, String> {
        self.with_connection().await?;
        let _ = self.eval(HIDE_OVERLAY_JS).await;
        if text.chars().count() > 8000 {
            return Err("That text is too long to type in one step.".into());
        }
        let focus = if let Some(index) = index {
            format!("document.querySelector('[data-wb=\"{index}\"]')")
        } else if let (Some(x), Some(y)) = (x, y) {
            let (cx, cy) = self.to_css(x, y);
            format!("document.elementFromPoint({cx}, {cy})")
        } else {
            return Err("Pass the field index, or x and y, plus the text to type.".into());
        };
        let quoted = serde_json::to_string(text).map_err(|e| e.to_string())?;
        let clear_js = if clear { "true" } else { "false" };
        let prep = self
            .eval(&format!(
                "(function(){{const el={focus};if(!el)return null;el.scrollIntoView({{block:'center',inline:'nearest'}});const r=el.getBoundingClientRect();el.focus();if({clear_js} && 'value' in el){{const proto=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;const desc=Object.getOwnPropertyDescriptor(proto,'value');if(desc&&desc.set)desc.set.call(el,'');else el.value='';el.dispatchEvent(new Event('input',{{bubbles:true}}));}}return {{x:r.left+r.width/2,y:r.top+r.height/2,tag:el.tagName}};}})()"
            ))
            .await?;
        if prep.is_null() {
            return Err("Could not find that field. Look at the page again.".into());
        }
        let cx = prep["x"].as_f64().unwrap_or(0.0);
        let cy = prep["y"].as_f64().unwrap_or(0.0);
        self.mouse_click(cx, cy, "left", 1).await?;
        if clear {
            self.chord("a", 2).await?;
            self.press_key("Backspace").await?;
        }
        self.conn_call("Input.insertText", json!({ "text": text })).await?;
        let check = self
            .eval(&format!(
                "(function(){{const el={focus};if(!el||!('value' in el))return {{ok:true,mode:'plain'}};const want={quoted};if(String(el.value).includes(want))return {{ok:true,mode:'insert'}};const proto=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;const desc=Object.getOwnPropertyDescriptor(proto,'value');if(desc&&desc.set)desc.set.call(el,want);else el.value=want;el.dispatchEvent(new InputEvent('input',{{bubbles:true,data:want,inputType:'insertText'}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return {{ok:String(el.value).includes(want),mode:'setter'}};}})()"
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

    pub async fn scroll(&mut self, dx: f64, dy: f64, x: Option<f64>, y: Option<f64>) -> Result<String, String> {
        self.with_connection().await?;
        let (cx, cy) = match (x, y) {
            (Some(x), Some(y)) => self.to_css(x, y),
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
        Ok(format!("Scrolled by ({dx:.0}, {dy:.0})."))
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

    pub async fn select_option(&mut self, index: i64, label: &str) -> Result<String, String> {
        self.with_connection().await?;
        let quoted = serde_json::to_string(label).map_err(|e| e.to_string())?;
        let result = self
            .eval(&format!(
                "(function(){{const el=document.querySelector('[data-wb=\"{index}\"]');if(!el||el.tagName!=='SELECT')return {{ok:false}};const want={quoted}.toLowerCase();const opt=[...el.options].find(o=>o.value.toLowerCase()===want||o.label.toLowerCase()===want||o.text.toLowerCase()===want);if(!opt)return {{ok:false,reason:'option'}};el.value=opt.value;el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return {{ok:true,value:opt.value,label:opt.label||opt.text}};}})()"
            ))
            .await?;
        if result["ok"].as_bool() != Some(true) {
            return Err(format!("Could not select \"{label}\"."));
        }
        Ok(format!(
            "Selected {}.",
            result["label"].as_str().unwrap_or(label)
        ))
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

    async fn point(&mut self, index: Option<i64>, x: Option<f64>, y: Option<f64>) -> Result<(f64, f64), String> {
        if let Some(index) = index {
            let point = self
                .eval(&format!(
                    "(function(){{const el=document.querySelector('[data-wb=\"{index}\"]');if(!el)return null;const r=el.getBoundingClientRect();return {{x:r.left+r.width/2,y:r.top+r.height/2}};}})()"
                ))
                .await?;
            if point.is_null() {
                return Err(format!("Element [{index}] is gone. Look again."));
            }
            return Ok((
                point["x"].as_f64().unwrap_or(0.0),
                point["y"].as_f64().unwrap_or(0.0),
            ));
        }
        match (x, y) {
            (Some(x), Some(y)) => Ok(self.to_css(x, y)),
            _ => Err("Pass an element index or screenshot x and y.".into()),
        }
    }

    fn to_css(&self, x: f64, y: f64) -> (f64, f64) {
        let Some(frame) = &self.frame else {
            return (x, y);
        };
        let sx = if frame.img_w > 0.0 { frame.css_w / frame.img_w } else { 1.0 };
        let sy = if frame.img_h > 0.0 { frame.css_h / frame.img_h } else { 1.0 };
        (x * sx, y * sy)
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

    async fn chord(&mut self, key: &str, modifiers: u32) -> Result<(), String> {
        let vk = if key.eq_ignore_ascii_case("a") { 65 } else { 0 };
        let code = format!("Key{}", key.to_uppercase());
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

const SNAPSHOT_JS: &str = r#"(function(){
  document.querySelectorAll('[data-wb]').forEach(function(el){ el.removeAttribute('data-wb'); });
  var old = document.getElementById('wb-overlay');
  if (old) old.remove();
  var selectors = 'a[href],button,input,textarea,select,summary,[role="button"],[role="link"],[role="textbox"],[role="checkbox"],[role="radio"],[role="combobox"],[role="menuitem"],[role="tab"],[role="switch"],[contenteditable="true"]';
  var nodes = Array.from(document.querySelectorAll(selectors));
  var vw = window.innerWidth;
  var vh = window.innerHeight;
  var items = [];
  for (var n = 0; n < nodes.length; n++) {
    var el = nodes[n];
    var r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4) continue;
    if (r.bottom < 0 || r.top > vh || r.right < 0 || r.left > vw) continue;
    var s = getComputedStyle(el);
    if (s.visibility === 'hidden' || s.display === 'none' || Number(s.opacity) === 0) continue;
    var id = items.length;
    el.setAttribute('data-wb', String(id));
    var typ = el.getAttribute('type') || '';
    var raw = ('value' in el) ? String(el.value || '') : '';
    if (typ === 'password' && raw) raw = '••••';
    var text = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.innerText || el.getAttribute('name') || el.getAttribute('alt') || '').replace(/\s+/g, ' ').trim().slice(0, 90);
    items.push({
      i: id,
      tag: el.tagName.toLowerCase(),
      type: typ.slice(0, 24),
      role: (el.getAttribute('role') || '').slice(0, 24),
      text: text,
      value: raw.slice(0, 80),
      href: (el.href || '').slice(0, 140),
      x: Math.round(r.left + r.width / 2),
      y: Math.round(r.top + r.height / 2),
      left: Math.round(r.left),
      top: Math.round(r.top)
    });
    if (items.length >= 70) break;
  }
  var overlay = document.createElement('div');
  overlay.id = 'wb-overlay';
  overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483646;pointer-events:none;';
  for (var j = 0; j < items.length; j++) {
    var item = items[j];
    var badge = document.createElement('div');
    badge.textContent = String(item.i);
    badge.style.cssText = 'position:fixed;left:' + Math.max(0, item.left) + 'px;top:' + Math.max(0, item.top) + 'px;z-index:2147483647;background:#1a1206;color:#ffd36a;font:700 11px/1.2 Segoe UI,sans-serif;padding:1px 4px;border-radius:4px;border:1px solid #ffd36a;pointer-events:none;';
    overlay.appendChild(badge);
  }
  document.documentElement.appendChild(overlay);
  var pageText = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 1500);
  return { url: location.href, title: document.title, vw: vw, vh: vh, items: items, pageText: pageText };
})()"#;

const FULL_SCAN_JS: &str = r#"(function(){
  document.querySelectorAll('[data-wb]').forEach(function(el){ el.removeAttribute('data-wb'); });
  var old = document.getElementById('wb-overlay');
  if (old) old.remove();
  var selectors = 'a[href],button,input,textarea,select,summary,[role="button"],[role="link"],[role="textbox"],[role="checkbox"],[role="radio"],[role="combobox"],[role="menuitem"],[role="tab"],[role="switch"],[contenteditable="true"]';
  var nodes = Array.from(document.querySelectorAll(selectors));
  var items = [];
  var overlay = document.createElement('div');
  overlay.id = 'wb-overlay';
  overlay.style.cssText = 'position:absolute;left:0;top:0;width:100%;height:100%;z-index:2147483646;pointer-events:none;';
  for (var n = 0; n < nodes.length; n++) {
    var el = nodes[n];
    var r = el.getBoundingClientRect();
    if (r.width < 4 || r.height < 4) continue;
    var s = getComputedStyle(el);
    if (s.visibility === 'hidden' || s.display === 'none' || Number(s.opacity) === 0) continue;
    var id = items.length;
    el.setAttribute('data-wb', String(id));
    var typ = el.getAttribute('type') || '';
    var raw = ('value' in el) ? String(el.value || '') : '';
    if (typ === 'password' && raw) raw = '••••';
    var text = (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.innerText || el.getAttribute('name') || el.getAttribute('alt') || '').replace(/\s+/g, ' ').trim().slice(0, 100);
    var x = Math.round(r.left + scrollX + r.width / 2);
    var y = Math.round(r.top + scrollY + r.height / 2);
    items.push({ i:id, tag:el.tagName.toLowerCase(), type:typ.slice(0,24), text:text, value:raw.slice(0,120), x:x, y:y });
    var badge = document.createElement('div');
    badge.textContent = String(id);
    badge.style.cssText = 'position:absolute;left:' + Math.max(0, Math.round(r.left + scrollX)) + 'px;top:' + Math.max(0, Math.round(r.top + scrollY)) + 'px;z-index:2147483647;background:#1a1206;color:#ffd36a;font:700 11px/1.2 Segoe UI,sans-serif;padding:1px 4px;border-radius:4px;border:1px solid #ffd36a;pointer-events:none;';
    overlay.appendChild(badge);
    if (items.length >= 180) break;
  }
  document.documentElement.appendChild(overlay);
  var width = Math.max(document.documentElement.scrollWidth, document.body ? document.body.scrollWidth : 0, innerWidth);
  var height = Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0, innerHeight);
  var pageText = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 12000);
  return { url:location.href, title:document.title, width:width, height:height, items:items, pageText:pageText };
})()"#;

const HIDE_OVERLAY_JS: &str = r#"(function(){var n=document.getElementById('wb-overlay');if(n)n.remove();return true;})()"#;

#[cfg(test)]
mod tests {
    use super::jpeg_size;
    use base64::Engine;
    use std::time::Duration;

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
                .is_final_action_target(Some(button), None, None)
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
}
