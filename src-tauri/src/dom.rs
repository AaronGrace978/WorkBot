//! Page-side scripts that give Work Bot a DevTools-style (F12) view of the DOM.
//!
//! Every script runs after `WB_CORE_JS`, which installs `window.__wb`:
//! element indexes that stay stable across observations for the life of the
//! document, lookup that crosses open Shadow DOM and same-origin iframes, and a
//! value setter that React, Vue, and Angular forms accept.

use serde_json::Value;

/// Prefix `body` with the core helpers. Indexes continue from `base` so a new
/// document never reuses numbers the model saw on the previous page.
pub fn with_core(base: u64, body: &str) -> String {
    format!(
        "{};\n{}",
        WB_CORE_JS.replace("__WB_BASE__", &base.max(1).to_string()),
        body
    )
}

pub fn with_arg(script: &str, arg: &Value) -> String {
    script.replace("__ARG__", &arg.to_string())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn kind_name(kind: &str) -> &str {
    match kind {
        "checkbox-role" => "checkbox",
        "radio-role" => "radio",
        "richtext" => "rich-text",
        "multiselect" => "multi-select",
        other => other,
    }
}

fn is_empty_value(field: &Value) -> bool {
    if text(field, "kind") == "radio" {
        return !field["options"]
            .as_array()
            .is_some_and(|options| options.iter().any(|o| flag(o, "checked")));
    }
    let value = text(field, "value");
    value.trim().is_empty() || value == "unchecked"
}

fn tag_of(field: &Value) -> String {
    format!(
        "[{}] {}",
        field["i"].as_i64().unwrap_or(0),
        text(field, "label")
    )
}

pub fn required_empty(map: &Value) -> Vec<String> {
    map["fields"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter(|f| flag(f, "req") && is_empty_value(f))
                .map(tag_of)
                .collect()
        })
        .unwrap_or_default()
}

fn field_errors(map: &Value) -> Vec<String> {
    map["fields"]
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter(|f| !text(f, "err").is_empty())
                .map(|f| format!("{}: \"{}\"", tag_of(f), text(f, "err")))
                .collect()
        })
        .unwrap_or_default()
}

fn field_line(field: &Value) -> String {
    let kind = text(field, "kind");
    let mut line = format!(
        "[{}] {} \"{}\"",
        field["i"].as_i64().unwrap_or(0),
        kind_name(kind),
        text(field, "label")
    );
    if flag(field, "req") {
        line.push_str(" *required");
    }
    match kind {
        "radio" => {
            let options = field["options"].as_array().cloned().unwrap_or_default();
            let listed: Vec<String> = options
                .iter()
                .map(|o| format!("[{}] {}", o["i"].as_i64().unwrap_or(0), text(o, "label")))
                .collect();
            let chosen = options
                .iter()
                .find(|o| flag(o, "checked"))
                .map(|o| format!("chosen: {}", text(o, "label")))
                .unwrap_or_else(|| "none chosen".into());
            line.push_str(&format!(" choices: {} ({chosen})", listed.join(" · ")));
        }
        "select" | "multiselect" => {
            line.push_str(&format!(" = \"{}\"", text(field, "value")));
            let options: Vec<&str> = field["options"]
                .as_array()
                .map(|o| o.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !options.is_empty() {
                let shown: Vec<&str> = options.iter().copied().take(30).collect();
                line.push_str(&format!(" options: {}", shown.join(" | ")));
                if options.len() > shown.len() {
                    line.push_str(&format!(
                        " … (+{} more; fill matches by text)",
                        options.len() - shown.len()
                    ));
                }
            }
        }
        "file" => {
            let value = text(field, "value");
            line.push_str(&format!(
                " = {}",
                if value.is_empty() { "no file".to_string() } else { format!("\"{value}\"") }
            ));
            if !text(field, "accept").is_empty() {
                line.push_str(&format!(" accept={}", text(field, "accept")));
            }
            line.push_str(" → attach with docs_attach");
        }
        _ => line.push_str(&format!(" = \"{}\"", text(field, "value"))),
    }
    if !text(field, "ph").is_empty() {
        line.push_str(&format!(" placeholder=\"{}\"", text(field, "ph")));
    }
    if let Some(max) = field["max"].as_i64() {
        line.push_str(&format!(" maxlength={max}"));
    }
    if !text(field, "pattern").is_empty() {
        line.push_str(&format!(" pattern={}", text(field, "pattern")));
    }
    if !text(field, "min").is_empty() || !text(field, "maxv").is_empty() {
        line.push_str(&format!(" range={}..{}", text(field, "min"), text(field, "maxv")));
    }
    if kind == "combobox" {
        line.push_str(" (dropdown/autocomplete: browser_fill_form opens it and picks the matching option)");
    }
    if flag(field, "dis") {
        line.push_str(" (disabled)");
    }
    if flag(field, "ro") {
        line.push_str(" (read-only)");
    }
    if flag(field, "hidden") && kind != "file" {
        line.push_str(" (styled control; filling by index still works)");
    }
    match text(field, "where") {
        "" => {}
        place => line.push_str(&format!(" ({place} the viewport)")),
    }
    line
}

/// Render the result of `FIELDS_JS` as a compact, model-friendly form map.
pub fn format_form_map(map: &Value) -> String {
    let fields = map["fields"].as_array().cloned().unwrap_or_default();
    let total = map["total"].as_u64().unwrap_or(fields.len() as u64) as usize;
    let mut lines = Vec::new();
    if fields.is_empty() {
        lines.push("FORM MAP — no fillable fields on this page.".to_string());
    } else {
        let missing = required_empty(map);
        lines.push(format!(
            "FORM MAP — {} field{}{}. Indexes stay valid on this page: fill or click any of them directly, no scrolling needed.",
            fields.len(),
            if fields.len() == 1 { "" } else { "s" },
            if missing.is_empty() {
                String::new()
            } else {
                format!(", {} required still empty", missing.len())
            }
        ));
        if total > fields.len() {
            let more = total - fields.len();
            lines.push(format!(
                "({more} more field{} not listed; fill these first, then call browser_fields.)",
                if more == 1 { " is" } else { "s are" }
            ));
        }
        let mut section = String::new();
        for field in &fields {
            let current = text(field, "section");
            if !current.is_empty() && current != section {
                if current != text(field, "label") {
                    lines.push(format!("— {current} —"));
                }
                section = current.to_string();
            }
            lines.push(field_line(field));
            if !text(field, "err").is_empty() {
                lines.push(format!("    error shown: \"{}\"", text(field, "err")));
            }
        }
    }
    if let Some(actions) = map["actions"].as_array().filter(|a| !a.is_empty()) {
        let listed: Vec<String> = actions
            .iter()
            .map(|a| {
                let mut item = format!("[{}] \"{}\"", a["i"].as_i64().unwrap_or(0), text(a, "text"));
                if flag(a, "final") {
                    item.push_str(" (final)");
                } else if flag(a, "next") {
                    item.push_str(" (next step)");
                }
                if flag(a, "dis") {
                    item.push_str(" (disabled)");
                }
                item
            })
            .collect();
        lines.push(format!("Buttons: {}", listed.join(" · ")));
    }
    lines.join("\n")
}

pub fn format_review(map: &Value) -> String {
    let mut lines = vec![
        format!("FORM REVIEW — {}", text(map, "title")),
        format!("url: {}", text(map, "url")),
        format_form_map(map),
    ];
    let missing = required_empty(map);
    if !missing.is_empty() {
        lines.push(format!("Required but empty: {}", missing.join(", ")));
    }
    let errors = field_errors(map);
    if !errors.is_empty() {
        lines.push(format!("Fields showing errors: {}", errors.join("; ")));
    }
    lines.push(
        "Compare every value above with the user's original request and reference images. Fix mismatches with browser_fill_form before the final click."
            .into(),
    );
    lines.join("\n")
}

/// Summarize per-field fill outcomes. Each result carries `i`, `label`,
/// `value`, `ok`, and optional `note`, `choices`, or `target`.
pub fn format_fill_results(results: &[Value]) -> String {
    let ok_count = results.iter().filter(|r| flag(r, "ok")).count();
    let mut lines = vec![format!("Filled {ok_count} of {} fields:", results.len())];
    for result in results {
        let who = match result["i"].as_i64() {
            Some(i) => format!("[{i}] \"{}\"", text(result, "label")),
            None => text(result, "target").to_string(),
        };
        let mut line = if flag(result, "ok") {
            format!("OK {who} = \"{}\"", text(result, "value"))
        } else {
            let mut failed = format!("FAILED {who}");
            if !text(result, "value").is_empty() {
                failed.push_str(&format!(" (now \"{}\")", text(result, "value")));
            }
            failed
        };
        if !text(result, "note").is_empty() {
            line.push_str(&format!(": {}", text(result, "note")));
        }
        if let Some(choices) = result["choices"].as_array().filter(|c| !c.is_empty()) {
            let names: Vec<&str> = choices.iter().filter_map(Value::as_str).take(20).collect();
            line.push_str(&format!(". Choices: {}", names.join(" | ")));
        }
        lines.push(line);
    }
    if ok_count < results.len() {
        lines.push(
            "Fix the FAILED fields: use the listed choices, browser_inspect to read their HTML, or browser_dom_edit if the page blocks input. Then call browser_review before submitting."
                .into(),
        );
    }
    lines.join("\n")
}

pub const WB_CORE_JS: &str = r##"(function () {
  const base = __WB_BASE__;
  if (window.__wb && window.__wb.v === 3) {
    if (window.__wb.next < base) window.__wb.next = base;
    return true;
  }
  const wb = { v: 3, els: Object.create(null), next: base };
  const clean = (s, n) => String(s == null ? '' : s).replace(/\s+/g, ' ').trim().slice(0, n || 120);
  wb.clean = clean;
  wb.CLICKABLE = 'a[href],button,input,textarea,select,summary,[role="button"],[role="link"],[role="textbox"],[role="searchbox"],[role="checkbox"],[role="radio"],[role="combobox"],[role="listbox"],[role="option"],[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"],[role="tab"],[role="switch"],[role="treeitem"],[contenteditable="true"],[contenteditable=""]';
  wb.FIELDS = 'input,textarea,select,[contenteditable=""],[contenteditable="true"],[role="textbox"],[role="searchbox"],[role="combobox"],[role="checkbox"],[role="switch"],[role="radio"],[role="spinbutton"],[role="slider"]';

  wb.id = (el) => {
    const a = el.getAttribute && el.getAttribute('data-wb');
    if (a && wb.els[a] === el) return Number(a);
    const id = wb.next++;
    try { el.setAttribute('data-wb', String(id)); } catch (e) {}
    wb.els[id] = el;
    return id;
  };
  wb.prune = () => {
    for (const key in wb.els) if (!wb.els[key].isConnected) delete wb.els[key];
  };
  wb.roots = () => {
    const out = [];
    const add = (root, depth) => {
      if (!root || depth > 5) return;
      out.push(root);
      const all = root.querySelectorAll ? root.querySelectorAll('*') : [];
      for (const el of all) {
        if (el.shadowRoot) add(el.shadowRoot, depth + 1);
        if (el.tagName === 'IFRAME' || el.tagName === 'FRAME') {
          let doc = null;
          try { doc = el.contentDocument; } catch (e) {}
          if (doc && doc.documentElement) add(doc, depth + 1);
        }
      }
    };
    add(document, 0);
    return out;
  };
  wb.queryAll = (sel) => {
    const out = [];
    for (const root of wb.roots()) {
      let found;
      try { found = root.querySelectorAll(sel); } catch (e) { throw new Error('Invalid CSS selector: ' + sel); }
      for (const el of found) out.push(el);
    }
    return out;
  };
  wb.get = (i) => {
    const el = wb.els[i];
    if (el && el.isConnected) return el;
    return wb.queryAll('[data-wb="' + Number(i) + '"]')[0] || null;
  };
  window.__wb_get = wb.get;
  wb.view = (el) => (el.ownerDocument && el.ownerDocument.defaultView) || window;
  wb.rect = (el) => {
    const r = el.getBoundingClientRect();
    let x = r.left, y = r.top, view = wb.view(el);
    while (view && view !== window && view.frameElement) {
      const f = view.frameElement, fr = f.getBoundingClientRect();
      x += fr.left + (f.clientLeft || 0);
      y += fr.top + (f.clientTop || 0);
      view = view.parent;
    }
    return { left: x, top: y, width: r.width, height: r.height, right: x + r.width, bottom: y + r.height };
  };
  wb.shown = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const s = wb.view(el).getComputedStyle(el);
    return !(s.visibility === 'hidden' || s.display === 'none' || Number(s.opacity) === 0);
  };
  // Scroll only when the target is not comfortably visible, and never animate:
  // smooth scrolling made screenshots land mid-scroll.
  wb.reveal = (el, force) => {
    let r = wb.rect(el);
    const vh = innerHeight, vw = innerWidth, margin = Math.min(80, vh * 0.12);
    const okY = r.height <= vh - margin
      ? (r.top >= margin && r.bottom <= vh - 8)
      : (r.top < vh / 2 && r.bottom > vh / 2);
    const okX = (r.left >= 0 && r.right <= vw) || (r.width > vw && r.left <= 0);
    if (force || !okY || !okX) {
      const view = wb.view(el);
      if (view !== window && view.frameElement) view.frameElement.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      r = wb.rect(el);
    }
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  };
  const composedContains = (outer, inner) => {
    for (let n = inner; n; n = n.parentNode || n.host) if (n === outer) return true;
    return false;
  };
  wb.describe = (el) => {
    if (!el || !el.tagName) return '';
    let d = el.tagName.toLowerCase();
    if (el.id) d += '#' + String(el.id).slice(0, 40);
    const cls = typeof el.className === 'string' ? el.className.trim().split(/\s+/).filter(Boolean).slice(0, 2) : [];
    if (cls.length) d += '.' + cls.join('.');
    const t = clean(el.getAttribute('aria-label') || el.innerText || el.value || '', 50);
    if (t) d += ' "' + t + '"';
    return d;
  };
  wb.blocker = (el, p) => {
    if (wb.view(el) !== window) return '';
    const hit = document.elementFromPoint(p.x, p.y);
    if (!hit || composedContains(el, hit) || composedContains(hit, el)) return '';
    const lab = hit.closest && hit.closest('label');
    if (lab && lab.control === el) return '';
    return wb.describe(hit);
  };
  wb.text = (node, n) => {
    let out = '';
    const walk = (x) => {
      if (out.length > 400) return;
      if (x.nodeType === 3) { out += x.nodeValue + ' '; return; }
      if (x.nodeType !== 1) return;
      if (/^(select|textarea|option|script|style|noscript|svg)$/i.test(x.tagName)) return;
      for (const c of x.childNodes) walk(c);
    };
    if (node) walk(node);
    return clean(out, n || 120);
  };
  wb.byIds = (el, ids) => {
    if (!ids) return '';
    const root = el.getRootNode ? el.getRootNode() : document;
    return ids.split(/\s+/).map((id) => {
      const n = (root.getElementById && root.getElementById(id)) || el.ownerDocument.getElementById(id);
      return n ? wb.text(n, 160) : '';
    }).join(' ');
  };
  wb.label = (el) => {
    let t = '';
    try { if (el.labels && el.labels.length) t = [...el.labels].map((l) => wb.text(l, 120)).join(' / '); } catch (e) {}
    if (!clean(t)) t = wb.byIds(el, el.getAttribute('aria-labelledby'));
    if (!clean(t)) t = el.getAttribute('aria-label') || '';
    if (!clean(t) && el.closest) { const lab = el.closest('label'); if (lab) t = wb.text(lab, 120); }
    if (!clean(t)) t = el.getAttribute('placeholder') || el.getAttribute('title') || '';
    if (!clean(t)) {
      let p = el;
      for (let k = 0; k < 3 && p && !clean(t); k++) {
        let sib = p.previousElementSibling;
        for (let s = 0; sib && s < 3 && !clean(t); s++) {
          if (!sib.querySelector || !sib.querySelector('input,select,textarea')) t = wb.text(sib, 120);
          sib = sib.previousElementSibling;
        }
        p = p.parentElement;
      }
    }
    if (!clean(t)) t = el.getAttribute('name') || el.id || '';
    return clean(t, 120);
  };
  wb.radios = (el) => {
    if (!el.name) return [el];
    const scope = el.form || el.getRootNode();
    return [...scope.querySelectorAll('input[type="radio"]')].filter((r) => r.name === el.name);
  };
  wb.groupLabel = (el, members) => {
    const fs = el.closest('fieldset');
    if (fs) { const lg = fs.querySelector('legend'); const t = wb.text(lg, 140); if (t) return t; }
    const rg = el.closest('[role="radiogroup"],[role="group"]');
    if (rg) { const t = clean(rg.getAttribute('aria-label') || wb.byIds(rg, rg.getAttribute('aria-labelledby')), 140); if (t) return t; }
    let box = el.parentElement;
    while (box && !members.every((m) => box.contains(m))) box = box.parentElement;
    const own = new Set(members.map((m) => wb.label(m)));
    for (let k = 0; box && k < 3; k++, box = box.parentElement) {
      const prev = box.previousElementSibling;
      const t = prev ? wb.text(prev, 140) : '';
      if (t && !own.has(t)) return t;
    }
    return clean(el.name, 60);
  };
  wb.kind = (el) => {
    const tag = el.tagName, role = (el.getAttribute('role') || '').toLowerCase();
    if (tag === 'SELECT') return el.multiple ? 'multiselect' : 'select';
    if (tag === 'TEXTAREA') return 'textarea';
    if (tag === 'INPUT') {
      const type = (el.getAttribute('type') || 'text').toLowerCase();
      if (role === 'combobox' && !/^(checkbox|radio|file|hidden|submit|button|reset|image)$/.test(type)) return 'combobox';
      return type;
    }
    if (el.isContentEditable || role === 'textbox' || role === 'searchbox') return 'richtext';
    if (role === 'combobox') return 'combobox';
    if (role === 'checkbox' || role === 'switch') return 'checkbox-role';
    if (role === 'radio') return 'radio-role';
    if (role === 'spinbutton' || role === 'slider') return role;
    return '';
  };
  wb.isField = (el) => {
    const k = wb.kind(el);
    return !!k && !/^(hidden|submit|button|reset|image)$/.test(k);
  };
  wb.display = (el) => {
    const kind = wb.kind(el);
    if (kind === 'select' || kind === 'multiselect') {
      return [...el.selectedOptions].filter((o) => o.value !== '' || el.multiple).map((o) => clean(o.text, 60)).join(' | ');
    }
    if (kind === 'checkbox' || kind === 'radio') return el.checked ? 'checked' : 'unchecked';
    if (kind === 'checkbox-role' || kind === 'radio-role') return el.getAttribute('aria-checked') === 'true' ? 'checked' : 'unchecked';
    if (kind === 'file') return el.files && el.files.length ? [...el.files].map((f) => f.name).join(', ') : '';
    if (kind === 'password') return el.value ? '••••' : '';
    if (kind === 'richtext') return clean(el.innerText, 200);
    if (kind === 'combobox') {
      if (el.tagName === 'INPUT' && clean(el.value)) return clean(el.value, 120);
      let p = el.tagName === 'INPUT' ? el.parentElement : el;
      for (let k = 0; p && k < 4; k++, p = p.parentElement) { const t = clean(p.innerText, 120); if (t) return t; }
      return '';
    }
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') return clean(el.value, 200);
    return clean(el.getAttribute('aria-valuetext') || el.getAttribute('aria-valuenow') || el.innerText, 120);
  };
  wb.errorText = (el) => {
    let t = '';
    const ids = ((el.getAttribute('aria-errormessage') || '') + ' ' + (el.getAttribute('aria-invalid') === 'true' ? (el.getAttribute('aria-describedby') || '') : '')).trim();
    if (ids) t = wb.byIds(el, ids);
    if (!clean(t) && 'validationMessage' in el && el.value && el.matches && el.matches(':invalid')) t = el.validationMessage;
    return clean(t, 120);
  };
  wb.item = (el, r) => {
    const field = wb.isField(el);
    const text = field ? wb.label(el) : (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.innerText || el.getAttribute('name') || el.getAttribute('alt') || '');
    return {
      i: wb.id(el),
      tag: el.tagName.toLowerCase(),
      type: (el.getAttribute('type') || '').slice(0, 24),
      role: (el.getAttribute('role') || '').slice(0, 24),
      text: clean(text, 90),
      value: field ? clean(wb.display(el), 80) : '',
      href: (typeof el.href === 'string' ? el.href : '').slice(0, 140),
      x: Math.round(r.left + r.width / 2),
      y: Math.round(r.top + r.height / 2),
      left: Math.round(r.left),
      top: Math.round(r.top),
    };
  };
  wb.badges = (items, position) => {
    const old = document.getElementById('wb-overlay');
    if (old) old.remove();
    const overlay = document.createElement('div');
    overlay.id = 'wb-overlay';
    overlay.style.cssText = position === 'fixed'
      ? 'position:fixed;inset:0;z-index:2147483646;pointer-events:none;'
      : 'position:absolute;left:0;top:0;width:100%;height:100%;z-index:2147483646;pointer-events:none;';
    for (const item of items) {
      const badge = document.createElement('div');
      badge.textContent = String(item.i);
      badge.style.cssText = 'position:' + position + ';left:' + Math.max(0, item.left) + 'px;top:' + Math.max(0, item.top) + 'px;z-index:2147483647;background:#1a1206;color:#ffd36a;font:700 11px/1.2 Segoe UI,sans-serif;padding:1px 4px;border-radius:4px;border:1px solid #ffd36a;pointer-events:none;';
      overlay.appendChild(badge);
    }
    document.documentElement.appendChild(overlay);
  };

  const norm = (s) => clean(s, 300).toLowerCase();
  const squash = (s) => norm(s).replace(/[^a-z0-9]/g, '');
  wb.match = (choices, want) => {
    const w = norm(want), ws = squash(want);
    if (!w) return null;
    const tests = [
      (c) => norm(c.value) === w,
      (c) => norm(c.text) === w,
      (c) => ws !== '' && squash(c.text) === ws,
      (c) => norm(c.text).startsWith(w),
      (c) => norm(c.text).includes(w),
    ];
    for (const test of tests) { const hit = choices.find(test); if (hit) return hit; }
    const contained = choices.filter((c) => { const t = norm(c.text); return t.length >= 2 && w.includes(t); });
    contained.sort((a, b) => norm(b.text).length - norm(a.text).length);
    return contained[0] || null;
  };
  const truthy = (v) => /^(true|yes|y|on|1|checked|check|x|selected)$/i.test(String(v).trim());
  const falsy = (v) => /^(false|no|n|off|0|unchecked|uncheck|none|)$/i.test(String(v).trim());
  const digits = (s) => String(s).replace(/\D/g, '');
  const pad = (n) => String(n).padStart(2, '0');
  wb.nativeSet = (el, v) => {
    const view = wb.view(el);
    const proto = el.tagName === 'TEXTAREA' ? view.HTMLTextAreaElement.prototype : view.HTMLInputElement.prototype;
    const desc = Object.getOwnPropertyDescriptor(proto, 'value');
    if (desc && desc.set) desc.set.call(el, v); else el.value = v;
  };
  const setChecked = (el, on) => {
    const desc = Object.getOwnPropertyDescriptor(wb.view(el).HTMLInputElement.prototype, 'checked');
    if (desc && desc.set) desc.set.call(el, on); else el.checked = on;
  };
  wb.fire = (el, type) => {
    const view = wb.view(el);
    const ev = type === 'input'
      ? new view.InputEvent('input', { bubbles: true, composed: true, inputType: 'insertText' })
      : new view.Event(type, { bubbles: true, composed: true });
    el.dispatchEvent(ev);
  };
  wb.normValue = (kind, v) => {
    const s = String(v == null ? '' : v).trim();
    let m;
    if (kind === 'number' || kind === 'range') return s.replace(/[^0-9.eE+\-]/g, '');
    if (kind === 'date') {
      if (/^\d{4}-\d{2}-\d{2}$/.test(s)) return s;
      if ((m = s.match(/^(\d{4})[\/.](\d{1,2})[\/.](\d{1,2})$/))) return m[1] + '-' + pad(m[2]) + '-' + pad(m[3]);
      if ((m = s.match(/^(\d{1,2})[\/.\-](\d{1,2})[\/.\-](\d{4})$/))) return m[3] + '-' + pad(m[1]) + '-' + pad(m[2]);
      const d = new Date(s);
      if (s && !isNaN(d)) return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate());
    }
    if (kind === 'month') {
      if (/^\d{4}-\d{2}$/.test(s)) return s;
      if ((m = s.match(/^(\d{1,2})[\/.\-](\d{4})$/))) return m[2] + '-' + pad(m[1]);
      const d = new Date(s);
      if (s && !isNaN(d)) return d.getFullYear() + '-' + pad(d.getMonth() + 1);
    }
    if (kind === 'time' && (m = s.match(/^(\d{1,2}):(\d{2})\s*([ap])\.?m\.?$/i))) {
      let h = Number(m[1]) % 12;
      if (/p/i.test(m[3])) h += 12;
      return pad(h) + ':' + m[2];
    }
    return s;
  };
  wb.verify = (el, v) => {
    const kind = wb.kind(el), shown = wb.display(el);
    if (kind === 'checkbox' || kind === 'checkbox-role') return { ok: (shown === 'checked') === !falsy(v), value: shown };
    if (kind === 'radio' || kind === 'radio-role') return { ok: shown === 'checked', value: wb.label(el) };
    if (kind === 'select' || kind === 'multiselect' || kind === 'combobox') {
      const s = norm(shown), w = norm(v);
      return { ok: !!w && (s === w || s.includes(w) || (s.length >= 2 && w.includes(s))), value: shown };
    }
    if (kind === 'password') return { ok: el.value === String(v), value: shown };
    if (kind === 'richtext') return { ok: norm(el.innerText).includes(norm(v).slice(0, 60)), value: shown };
    const want = wb.normValue(kind, v), got = String(el.value == null ? shown : el.value);
    const ok = got === want || norm(got) === norm(want) || (digits(want).length >= 6 && digits(got) === digits(want));
    return { ok, value: shown };
  };
  wb.fill = (el, value) => {
    const kind = wb.kind(el);
    const v = value == null ? '' : String(value);
    const tag = el.tagName.toLowerCase();
    if (!kind) return { kind: tag, tag, ok: false, note: 'not a form field; use browser_click or browser_dom_edit' };
    if (kind === 'file') return { kind, tag, ok: false, note: 'file input: attach with docs_attach' };
    if (/^(submit|button|reset|image|hidden)$/.test(kind)) return { kind, tag, ok: false, note: 'not a fillable field' };
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') return { kind, tag, ok: false, note: 'disabled; browser_dom_edit remove_attribute disabled can enable it if appropriate' };
    if (kind === 'select' || kind === 'multiselect') {
      const opts = [...el.options].map((o) => ({ el: o, text: clean(o.text, 200), value: o.value }));
      const wants = kind === 'multiselect' ? v.split(/\s*[|;]\s*/).filter(Boolean) : [v];
      const picked = [];
      for (const w of wants) {
        const hit = wb.match(opts, w);
        if (!hit) return { kind, tag, ok: false, note: 'no option matches "' + w + '"', choices: opts.map((o) => o.text).filter(Boolean).slice(0, 30) };
        picked.push(hit);
      }
      try { el.focus({ preventScroll: true }); } catch (e) {}
      if (kind === 'multiselect') { for (const o of opts) o.el.selected = picked.includes(o); }
      else el.selectedIndex = picked[0].el.index;
      wb.fire(el, 'input');
      wb.fire(el, 'change');
      try { el.blur(); } catch (e) {}
      return { kind, tag, ok: true, value: wb.display(el) };
    }
    if (kind === 'checkbox') {
      const want = !falsy(v);
      if (el.checked !== want) el.click();
      if (el.checked !== want) { setChecked(el, want); wb.fire(el, 'input'); wb.fire(el, 'change'); }
      return { kind, tag, ok: el.checked === want, value: wb.display(el) };
    }
    if (kind === 'radio') {
      const members = wb.radios(el);
      const choices = members.map((r) => ({ el: r, text: wb.label(r), value: r.value }));
      const hit = wb.match(choices, v);
      const target = hit ? hit.el : (members.length === 1 || truthy(v) ? el : null);
      if (!target) return { kind, tag, ok: false, note: 'no choice matches "' + v + '"', choices: choices.map((c) => c.text) };
      if (!target.checked) target.click();
      if (!target.checked) { setChecked(target, true); wb.fire(target, 'input'); wb.fire(target, 'change'); }
      return { kind, tag, ok: target.checked, value: wb.label(target), pick: wb.id(target) };
    }
    if (kind === 'checkbox-role') {
      const want = !falsy(v), on = () => el.getAttribute('aria-checked') === 'true';
      if (on() !== want) el.click();
      return { kind, tag, ok: on() === want, value: on() ? 'checked' : 'unchecked', trusted: on() === want ? null : wb.id(el) };
    }
    if (kind === 'radio-role') {
      const group = el.closest('[role="radiogroup"]') || el.parentElement;
      const members = group ? [...group.querySelectorAll('[role="radio"]')] : [el];
      const choices = members.map((r) => ({ el: r, text: wb.label(r) || clean(r.innerText, 80), value: r.getAttribute('data-value') || r.getAttribute('value') || '' }));
      const hit = wb.match(choices, v);
      const target = hit ? hit.el : (truthy(v) ? el : null);
      if (!target) return { kind, tag, ok: false, note: 'no choice matches "' + v + '"', choices: choices.map((c) => c.text) };
      if (target.getAttribute('aria-checked') !== 'true') target.click();
      const ok = target.getAttribute('aria-checked') === 'true';
      return { kind, tag, ok, value: wb.label(target) || clean(target.innerText, 60), pick: wb.id(target), trusted: ok ? null : wb.id(target) };
    }
    if (kind === 'richtext') {
      const doc = el.ownerDocument;
      try { el.focus({ preventScroll: true }); } catch (e) {}
      try {
        doc.getSelection().selectAllChildren(el);
        doc.execCommand(v ? 'insertText' : 'delete', false, v);
      } catch (e) {}
      const check = wb.verify(el, v);
      return { kind, tag, ok: check.ok, value: check.value, retype: !check.ok };
    }
    if (kind === 'combobox') return { kind, tag, ok: false, custom: true };
    if (tag !== 'input' && tag !== 'textarea') return { kind, tag, ok: false, note: 'custom ' + kind + ': use browser_drag or arrow keys' };
    if (el.readOnly) return { kind, tag, ok: false, note: 'read-only: a picker widget probably controls it. Click it and choose, or remove readonly with browser_dom_edit' };
    const want = wb.normValue(kind, v);
    try { el.focus({ preventScroll: true }); } catch (e) {}
    wb.nativeSet(el, want);
    wb.fire(el, 'input');
    wb.fire(el, 'change');
    try { el.blur(); } catch (e) {}
    const check = wb.verify(el, v);
    return { kind, tag, ok: check.ok, value: check.value, retype: !check.ok };
  };
  wb.fieldChoices = () => {
    const out = [];
    for (const root of wb.roots()) {
      for (const el of root.querySelectorAll(wb.FIELDS)) {
        if (!wb.isField(el)) continue;
        const kind = wb.kind(el);
        const text = kind === 'radio' ? wb.groupLabel(el, wb.radios(el)) : wb.label(el);
        out.push({ el, text, value: el.getAttribute('name') || '' });
      }
    }
    return out;
  };
  window.__wb = wb;
  return true;
})()"##;

pub const SNAPSHOT_JS: &str = r##"(function () {
  const wb = window.__wb, clean = wb.clean;
  wb.prune();
  const vw = innerWidth, vh = innerHeight, items = [], off = [];
  for (const root of wb.roots()) {
    for (const el of root.querySelectorAll(wb.CLICKABLE)) {
      if (el.type === 'hidden' || !wb.shown(el)) continue;
      const r = wb.rect(el);
      const inView = r.bottom > 0 && r.top < vh && r.right > 0 && r.left < vw;
      if (!inView) {
        if (wb.isField(el) && off.length < 40) {
          off.push({ i: wb.id(el), kind: wb.kind(el), label: wb.label(el), value: clean(wb.display(el), 60), dir: r.bottom <= 0 ? 'above' : r.top >= vh ? 'below' : 'beside' });
        }
        continue;
      }
      if (items.length < 90) items.push(wb.item(el, r));
    }
  }
  wb.badges(items, 'fixed');
  const se = document.scrollingElement || document.documentElement;
  const pageText = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 1800);
  return { url: location.href, title: document.title, vw, vh, items, off, scroll: { y: Math.round(scrollY), h: Math.round(se.scrollHeight) }, pageText, next: wb.next };
})()"##;

pub const FULL_SCAN_JS: &str = r##"(function () {
  const wb = window.__wb;
  wb.prune();
  const sx = scrollX, sy = scrollY, vh = innerHeight, items = [];
  outer: for (const root of wb.roots()) {
    for (const el of root.querySelectorAll(wb.CLICKABLE)) {
      if (items.length >= 220) break outer;
      if (el.type === 'hidden' || !wb.shown(el)) continue;
      const r = wb.rect(el);
      const it = wb.item(el, r);
      it.x += sx; it.y += sy; it.left += sx; it.top += sy;
      it.f = wb.isField(el);
      it.view = r.bottom > 0 && r.top < vh;
      items.push(it);
    }
  }
  wb.badges(items, 'absolute');
  const se = document.scrollingElement || document.documentElement;
  const width = Math.max(se.scrollWidth, document.body ? document.body.scrollWidth : 0, innerWidth);
  const height = Math.max(se.scrollHeight, document.body ? document.body.scrollHeight : 0, innerHeight);
  const pageText = (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').slice(0, 14000);
  return { url: location.href, title: document.title, vw: innerWidth, vh, sx: Math.round(sx), sy: Math.round(sy), width, height, items, pageText, next: wb.next };
})()"##;

pub const FIELDS_JS: &str = r##"(function () {
  const wb = window.__wb, clean = wb.clean;
  const ACT = 'button,input[type="submit"],input[type="button"],input[type="image"],[role="button"]';
  const FINAL = /\b(submit|send|apply|save|confirm|finish|complete|publish|post|place order|pay|purchase|sign up|register|create account)\b/i;
  const NEXT = /\b(next|continue|proceed)\b/i;
  const heads = [];
  for (const h of document.querySelectorAll('h1,h2,h3,h4,legend,[role="heading"]')) {
    const t = wb.text(h, 80);
    if (t && wb.shown(h)) heads.push({ el: h, t });
  }
  const sectionOf = (el) => {
    const fs = el.closest && el.closest('fieldset');
    if (fs) { const t = wb.text(fs.querySelector('legend'), 80); if (t) return t; }
    if (el.ownerDocument !== document) return '';
    let best = '';
    for (const h of heads) {
      const pos = h.el.compareDocumentPosition(el);
      if (pos & Node.DOCUMENT_POSITION_DISCONNECTED) return '';
      if (pos & Node.DOCUMENT_POSITION_FOLLOWING) best = h.t; else break;
    }
    return best;
  };
  const fields = [], scopes = new Map(), seen = new Set();
  let total = 0;
  for (const root of wb.roots()) {
    for (const el of root.querySelectorAll(wb.FIELDS)) {
      if (seen.has(el)) continue;
      seen.add(el);
      const kind = wb.kind(el);
      if (!wb.isField(el)) continue;
      if (kind === 'richtext' && el.parentElement && el.parentElement.isContentEditable) continue;
      if (/-role$/.test(kind) && el.querySelector('input,select,textarea')) continue;
      const shown = wb.shown(el);
      const proxy = !shown && (kind === 'checkbox' || kind === 'radio') &&
        ((el.labels && [...el.labels].some(wb.shown)) || (el.parentElement && wb.shown(el.parentElement)));
      if (!shown && !proxy && kind !== 'file') continue;
      total++;
      if (fields.length >= 160) continue;
      const id = wb.id(el);
      const r = wb.rect(el);
      const where = r.bottom <= 0 ? 'above' : r.top >= innerHeight ? 'below' : '';
      const req = !!el.required || el.getAttribute('aria-required') === 'true';
      if (kind === 'radio' && el.name) {
        const scope = el.form || el.getRootNode();
        let groups = scopes.get(scope);
        if (!groups) { groups = new Map(); scopes.set(scope, groups); }
        const existing = groups.get(el.name);
        if (existing) {
          total--;
          existing.options.push({ i: id, label: wb.label(el), checked: el.checked });
          if (req) existing.req = true;
          continue;
        }
        const entry = { i: id, kind, label: wb.groupLabel(el, wb.radios(el)), section: sectionOf(el), req, where, options: [{ i: id, label: wb.label(el), checked: el.checked }] };
        groups.set(el.name, entry);
        fields.push(entry);
        continue;
      }
      const entry = {
        i: id, kind, label: wb.label(el), section: sectionOf(el), req, where,
        dis: !!el.disabled || el.getAttribute('aria-disabled') === 'true',
        ro: !!el.readOnly && kind !== 'checkbox' && kind !== 'radio',
        hidden: !shown,
        value: wb.display(el),
      };
      if (el.tagName === 'SELECT') entry.options = [...el.options].map((o) => clean(o.text, 60)).filter(Boolean);
      const ph = clean(el.getAttribute('placeholder'), 60);
      if (ph && ph !== entry.label) entry.ph = ph;
      if (el.maxLength > 0 && el.maxLength < 100000) entry.max = el.maxLength;
      const pattern = el.getAttribute('pattern');
      if (pattern) entry.pattern = pattern.slice(0, 60);
      if (el.tagName === 'INPUT') {
        if (el.min) entry.min = el.min;
        if (el.max) entry.maxv = el.max;
        if (kind === 'file') entry.accept = el.accept || '';
      }
      const err = wb.errorText(el);
      if (err) entry.err = err;
      fields.push(entry);
    }
  }
  const actions = [];
  outer: for (const root of wb.roots()) {
    for (const b of root.querySelectorAll(ACT)) {
      if (actions.length >= 25) break outer;
      if (!wb.shown(b)) continue;
      const text = clean(b.innerText || b.value || b.getAttribute('aria-label') || b.title, 60);
      if (!text) continue;
      actions.push({ i: wb.id(b), text, final: b.getAttribute('type') === 'submit' || FINAL.test(text), next: NEXT.test(text), dis: !!b.disabled || b.getAttribute('aria-disabled') === 'true' });
    }
  }
  const se = document.scrollingElement || document.documentElement;
  return { url: location.href, title: document.title, fields, actions, total, scroll: { y: Math.round(scrollY), h: Math.round(se.scrollHeight), vh: innerHeight }, next: wb.next };
})()"##;

pub const FILL_JS: &str = r##"(function () {
  const wb = window.__wb, specs = __ARG__, out = [];
  for (const s of specs) {
    let res, el = null, target = '';
    try {
      if (s.index != null) { el = wb.get(s.index); target = '[' + s.index + ']'; }
      else if (s.selector) { el = wb.queryAll(s.selector)[0] || null; target = s.selector; }
      else if (s.label) { const hit = wb.match(wb.fieldChoices(), s.label); el = hit ? hit.el : null; target = '"' + s.label + '"'; }
      if (!el) { out.push({ target, ok: false, note: 'not found; call browser_fields for current indexes' }); continue; }
      res = wb.fill(el, s.value);
    } catch (e) {
      res = { ok: false, note: String((e && e.message) || e) };
    }
    res.target = target;
    if (el) {
      res.i = wb.id(el);
      res.label = res.kind === 'radio' ? wb.groupLabel(el, wb.radios(el)) : wb.label(el);
    }
    out.push(res);
  }
  return { results: out, next: wb.next };
})()"##;

pub const VERIFY_JS: &str = r##"(function () {
  const wb = window.__wb, a = __ARG__;
  const el = wb.get(a.i);
  if (!el) return { ok: false, value: '(element is gone)' };
  return wb.verify(el, a.value);
})()"##;

pub const OPTION_FIND_JS: &str = r##"(function () {
  const wb = window.__wb, a = __ARG__;
  const SEL = '[role="option"],[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"],[role="treeitem"],li[data-value],[data-automation-id="promptOption"]';
  const choices = [];
  outer: for (const root of wb.roots()) {
    for (const el of root.querySelectorAll(SEL)) {
      if (choices.length >= 300) break outer;
      if (!wb.shown(el)) continue;
      const text = wb.clean(el.innerText || el.getAttribute('aria-label') || el.textContent, 160);
      if (!text) continue;
      choices.push({ el, text, value: el.getAttribute('data-value') || '' });
    }
  }
  const hit = wb.match(choices, a.value);
  let active = document.activeElement;
  while (active && active.shadowRoot && active.shadowRoot.activeElement) active = active.shadowRoot.activeElement;
  const focusInput = !!active && (active.tagName === 'INPUT' || active.isContentEditable);
  return { pick: hit ? wb.id(hit.el) : null, text: hit ? hit.text : '', count: choices.length, choices: choices.slice(0, 15).map((c) => c.text), focusInput, next: wb.next };
})()"##;

pub const INSPECT_JS: &str = r##"(function () {
  const wb = window.__wb, clean = wb.clean, a = __ARG__;
  let root = null, what = '';
  if (a.index != null) { root = wb.get(a.index); what = 'element [' + a.index + ']'; }
  else if (a.selector) { root = wb.queryAll(a.selector)[0] || null; what = a.selector; }
  else {
    let best = null, most = 0;
    for (const r of wb.roots()) {
      for (const f of (r.querySelectorAll ? r.querySelectorAll('form') : [])) {
        const n = f.querySelectorAll('input,select,textarea').length;
        if (n > most) { best = f; most = n; }
      }
    }
    root = best || document.body;
    what = best ? 'the main form' : 'the page body';
  }
  if (!root) return { error: (a.index != null ? 'Element [' + a.index + '] is not on the page.' : 'Nothing matches ' + a.selector) + ' Call browser_fields or browser_look for current indexes.' };
  if (root.matches && root.matches(wb.CLICKABLE)) wb.id(root);
  for (const el of root.querySelectorAll(wb.CLICKABLE)) wb.id(el);
  const KEEP = /^(id|class|name|type|placeholder|role|for|href|src|action|method|required|disabled|readonly|checked|selected|multiple|maxlength|minlength|pattern|min|max|step|accept|autocomplete|contenteditable|hidden|title|alt|tabindex|data-wb|data-testid|data-test|data-qa|data-automation-id|aria-[a-z-]+)$/;
  const SKIP = { SCRIPT: 1, STYLE: 1, NOSCRIPT: 1, TEMPLATE: 1, LINK: 1, META: 1 };
  const out = [];
  let size = 0, cut = false;
  const line = (depth, s) => {
    if (cut) return;
    const row = '  '.repeat(Math.min(depth, 30)) + s;
    if (size + row.length > a.max) { out.push('… (truncated; inspect a smaller part with index or selector)'); cut = true; return; }
    out.push(row);
    size += row.length + 1;
  };
  const meaningful = (el) => [...el.attributes].some((at) => at.name !== 'class' && at.name !== 'style' && KEEP.test(at.name));
  const attrs = (el) => {
    const parts = [];
    for (const at of el.attributes) {
      if (!KEEP.test(at.name)) continue;
      let v = at.value;
      if (at.name === 'class') v = v.trim().split(/\s+/).slice(0, 4).join(' ');
      v = clean(v, at.name === 'href' || at.name === 'src' ? 100 : 80).replace(/"/g, '&quot;');
      parts.push(v === '' ? at.name : at.name + '="' + v + '"');
    }
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
      const t = (el.type || '').toLowerCase();
      if (t === 'checkbox' || t === 'radio') { if (el.checked && !el.hasAttribute('checked')) parts.push('checked'); }
      else if (t !== 'file') parts.push('value="' + (t === 'password' ? (el.value ? '••••' : '') : clean(el.value, 120).replace(/"/g, '&quot;')) + '"');
    }
    const s = wb.view(el).getComputedStyle(el);
    if (s.display === 'none') parts.push('[display:none]');
    else if (s.visibility === 'hidden') parts.push('[visibility:hidden]');
    else if (Number(s.opacity) === 0) parts.push('[opacity:0]');
    return parts.length ? ' ' + parts.join(' ') : '';
  };
  const walk = (node, depth) => {
    if (cut) return;
    if (node.nodeType === 3) { const t = clean(node.nodeValue, 160); if (t) line(depth, t); return; }
    if (node.nodeType !== 1) return;
    const el = node, tag = el.tagName.toLowerCase();
    if (SKIP[el.tagName] || el.id === 'wb-overlay') return;
    if (tag === 'svg') { line(depth, '<svg/>'); return; }
    const kids = [...el.childNodes].filter((k) => k.nodeType === 1 || (k.nodeType === 3 && clean(k.nodeValue)));
    if ((tag === 'div' || tag === 'span') && !el.shadowRoot && !meaningful(el) && kids.length === 1 && kids[0].nodeType === 1) { walk(kids[0], depth); return; }
    const open = '<' + tag + attrs(el) + '>';
    if (tag === 'select') {
      line(depth, open);
      const opts = [...el.options];
      for (const o of opts.slice(0, 20)) line(depth + 1, '<option value="' + clean(o.value, 60) + '"' + (o.selected ? ' selected' : '') + '>' + clean(o.text, 80) + '</option>');
      if (opts.length > 20) line(depth + 1, '… ' + (opts.length - 20) + ' more options');
      line(depth, '</select>');
      return;
    }
    if (!el.shadowRoot && tag !== 'iframe' && kids.length === 1 && kids[0].nodeType === 3) { line(depth, open + clean(kids[0].nodeValue, 160) + '</' + tag + '>'); return; }
    if (!el.shadowRoot && tag !== 'iframe' && !kids.length) { line(depth, open); return; }
    line(depth, open);
    if (el.shadowRoot) { line(depth + 1, '#shadow-root'); for (const k of el.shadowRoot.childNodes) walk(k, depth + 2); }
    if (tag === 'iframe') {
      let doc = null;
      try { doc = el.contentDocument; } catch (e) {}
      if (doc && doc.body) { line(depth + 1, '#document'); walk(doc.body, depth + 2); }
      else line(depth + 1, '(cross-origin frame: contents are not readable)');
    }
    for (const k of kids) walk(k, depth + 1);
    line(depth, '</' + tag + '>');
  };
  walk(root, 0);
  return { what, html: out.join('\n'), next: wb.next };
})()"##;

pub const DOM_EDIT_JS: &str = r##"(function () {
  const wb = window.__wb, a = __ARG__;
  let els = [];
  if (a.index != null) { const el = wb.get(a.index); if (el) els = [el]; }
  else if (a.selector) els = wb.queryAll(a.selector);
  if (!els.length) return { ok: false, error: a.index != null ? 'Element [' + a.index + '] is not on the page. Look again.' : 'Nothing matches ' + a.selector + '.' };
  els = a.all ? els.slice(0, 50) : els.slice(0, 1);
  const name = String(a.name || '').trim(), lname = name.toLowerCase(), val = a.value == null ? '' : String(a.value);
  if (a.action === 'set_attribute' || a.action === 'remove_attribute' || a.action === 'set_style') {
    if (!name) return { ok: false, error: a.action + ' needs a name.' };
  }
  if (a.action === 'set_attribute') {
    if (/^on/.test(lname) || lname === 'srcdoc') return { ok: false, error: 'Event-handler and srcdoc attributes cannot be set.' };
    if (/^(href|src|action|formaction|xlink:href|data)$/.test(lname) && /^\s*(javascript|vbscript|data):/i.test(val)) return { ok: false, error: 'Script URLs are not allowed.' };
  }
  if (a.action === 'set_style' && /(javascript:|expression\s*\()/i.test(val)) return { ok: false, error: 'That style value is not allowed.' };
  const done = [];
  for (const el of els) {
    if (/^(HTML|HEAD|BODY)$/.test(el.tagName) && /^(remove|hide|set_text)$/.test(a.action)) return { ok: false, error: 'Refusing to ' + a.action + ' the whole <' + el.tagName.toLowerCase() + '>.' };
    const view = wb.view(el);
    switch (a.action) {
      case 'set_attribute': el.setAttribute(name, val); break;
      case 'remove_attribute': el.removeAttribute(name); break;
      case 'set_text': el.textContent = val; wb.fire(el, 'input'); break;
      case 'set_style': el.style.setProperty(name, val, 'important'); break;
      case 'hide': el.style.setProperty('display', 'none', 'important'); break;
      case 'show': {
        el.removeAttribute('hidden');
        const s = view.getComputedStyle(el);
        if (s.display === 'none') el.style.setProperty('display', /^(SPAN|A|LABEL|INPUT|BUTTON|SELECT|TEXTAREA)$/.test(el.tagName) ? 'inline-block' : 'block', 'important');
        if (s.visibility === 'hidden') el.style.setProperty('visibility', 'visible', 'important');
        if (Number(s.opacity) === 0) el.style.setProperty('opacity', '1', 'important');
        const r = el.getBoundingClientRect();
        if (r.width < 4 || r.height < 4) {
          el.style.setProperty('position', 'static', 'important');
          el.style.setProperty('width', 'auto', 'important');
          el.style.setProperty('height', 'auto', 'important');
          el.style.setProperty('clip', 'auto', 'important');
          el.style.setProperty('clip-path', 'none', 'important');
        }
        break;
      }
      case 'remove': el.remove(); break;
      case 'scroll_into_view': el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' }); break;
      case 'focus': try { el.focus({ preventScroll: false }); } catch (e) {} break;
      case 'js_click': el.click(); break;
      default: return { ok: false, error: 'Unknown action ' + a.action + '.' };
    }
    done.push((el.isConnected ? '[' + wb.id(el) + '] ' : '') + wb.describe(el));
  }
  return { ok: true, count: done.length, items: done, next: wb.next };
})()"##;

pub const SCROLL_INFO_JS: &str = r##"(function () {
  const a = __ARG__;
  const se = document.scrollingElement || document.documentElement;
  let inner = null;
  if (a && a.x != null) {
    let el = document.elementFromPoint(a.x, a.y);
    while (el && el !== document.body && el !== document.documentElement) {
      const s = getComputedStyle(el);
      if (/(auto|scroll|overlay)/.test(s.overflowY + ' ' + s.overflowX) && (el.scrollHeight > el.clientHeight + 2 || el.scrollWidth > el.clientWidth + 2)) {
        inner = { top: Math.round(el.scrollTop), left: Math.round(el.scrollLeft), h: el.scrollHeight, ch: el.clientHeight };
        break;
      }
      el = el.parentElement;
    }
  }
  return { y: Math.round(scrollY), x: Math.round(scrollX), h: Math.round(se.scrollHeight), vh: innerHeight, inner };
})()"##;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_map() -> Value {
        json!({
            "title": "Apply",
            "url": "https://jobs.example.com/apply",
            "total": 4,
            "fields": [
                {"i": 12, "kind": "text", "label": "First name", "section": "Personal", "req": true, "value": ""},
                {"i": 13, "kind": "email", "label": "Email", "section": "Personal", "req": true, "value": "ada@example.com", "where": "below"},
                {"i": 15, "kind": "select", "label": "Country", "section": "Location", "req": false, "value": "", "options": ["Select…", "United States", "Canada"]},
                {"i": 18, "kind": "radio", "label": "Authorized to work?", "section": "Location", "req": true,
                 "options": [{"i": 18, "label": "Yes", "checked": false}, {"i": 19, "label": "No", "checked": false}],
                 "err": "Please choose one"}
            ],
            "actions": [{"i": 30, "text": "Submit application", "final": true}, {"i": 31, "text": "Next", "next": true}]
        })
    }

    #[test]
    fn form_map_lists_sections_choices_and_buttons() {
        let text = format_form_map(&sample_map());
        assert!(text.contains("FORM MAP — 4 fields, 2 required still empty"), "{text}");
        assert!(text.contains("— Personal —"));
        assert!(text.contains("[13] email \"Email\" *required = \"ada@example.com\""));
        assert!(text.contains("(below the viewport)"));
        assert!(text.contains("options: Select… | United States | Canada"));
        assert!(text.contains("choices: [18] Yes · [19] No (none chosen)"));
        assert!(text.contains("error shown: \"Please choose one\""));
        assert!(text.contains("[30] \"Submit application\" (final)"));
        assert!(text.contains("[31] \"Next\" (next step)"));
    }

    #[test]
    fn review_calls_out_missing_required_fields() {
        let text = format_review(&sample_map());
        assert!(text.contains("Required but empty: [12] First name, [18] Authorized to work?"), "{text}");
        assert!(text.contains("Fields showing errors: [18] Authorized to work?"));
    }

    #[test]
    fn fill_results_report_failures_with_choices() {
        let results = vec![
            json!({"i": 12, "label": "First name", "ok": true, "value": "Ada"}),
            json!({"i": 15, "label": "Country", "ok": false, "note": "no option matches \"USA\"", "choices": ["United States", "Canada"]}),
            json!({"target": "#missing", "ok": false, "note": "not found"}),
        ];
        let text = format_fill_results(&results);
        assert!(text.starts_with("Filled 1 of 3 fields:"));
        assert!(text.contains("OK [12] \"First name\" = \"Ada\""));
        assert!(text.contains("FAILED [15] \"Country\": no option matches \"USA\". Choices: United States | Canada"));
        assert!(text.contains("FAILED #missing: not found"));
        assert!(text.contains("Fix the FAILED fields"));
    }

    #[test]
    fn core_keeps_numbering_from_base() {
        let script = with_core(41, "1");
        assert!(script.contains("const base = 41;"));
        assert!(with_arg("f(__ARG__)", &json!({"a": 1})).contains("f({\"a\":1})"));
    }
}
