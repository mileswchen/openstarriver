//! Page-side JavaScript executed through Runtime.evaluate (DevTools
//! context — bypasses page CSP). Every script returns `JSON.stringify(...)`
//! of a plain object so the Rust side only ever deserializes a string,
//! independent of CDP remote-object wrapping.
//!
//! The snapshot walker keeps an id → element map in `window.__sr_refs`;
//! `click`/`type` look elements up there. A navigation (or any DOM
//! replacement) invalidates the map — actions then report `stale_ref`, and
//! the model is told to re-snapshot. Item shapes are documented in
//! [`crate::axtree`].

/// Full DOM walk → snapshot payload (see axtree item shapes). Refs are
/// assigned to interactive elements only, capped at 500 refs / 2400 nodes /
/// depth 24 so pathological pages can't hang the evaluate.
pub fn snapshot_js() -> &'static str {
    r#"
(() => {
  const refs = new Map();
  window.__sr_refs = refs;
  const MAX_NODES = 2400, MAX_ITEMS = 500, MAX_TEXT = 120, MAX_NAME = 100;
  const SKIP = new Set(['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','LINK','META','HEAD','TITLE','BASE']);
  const STRUCT_ROLES = new Set(['banner','main','navigation','contentinfo','complementary','region','form','search','dialog','alert','article','group','table','list','listitem','menu','menubar','tablist','tab']);
  const INTERACTIVE_TAGS = new Set(['A','BUTTON','INPUT','SELECT','TEXTAREA','SUMMARY','IMG','OPTION']);
  const INTERACTIVE_ROLES = new Set(['link','button','textbox','checkbox','radio','switch','menuitem','menuitemcheckbox','menuitemradio','tab','option','combobox','listbox','searchbox','slider','spinbutton','img','progressbar','scrollbar','textbox','treeitem']);
  const isInteractive = (el, role) => INTERACTIVE_TAGS.has(el.tagName)
    || INTERACTIVE_ROLES.has(role)
    || el.hasAttribute('onclick')
    || (el.hasAttribute('tabindex') && el.getAttribute('tabindex') !== '-1');
  const implicitRole = (el) => {
    switch (el.tagName) {
      case 'A': return 'link';
      case 'BUTTON': return 'button';
      case 'SUMMARY': return 'summary';
      case 'OPTION': return 'option';
      case 'IMG': return 'img';
      case 'FORM': return 'form';
      case 'LABEL': return 'label';
      case 'TEXTAREA': return 'textbox';
      case 'SELECT': return 'combobox';
      case 'INPUT': {
        const t = (el.getAttribute('type') || 'text').toLowerCase();
        if (t === 'checkbox' || t === 'radio') return t;
        if (t === 'submit' || t === 'button' || t === 'reset' || t === 'image') return 'button';
        return 'textbox';
      }
      default: return null;
    }
  };
  const nameOf = (el) => {
    const n = el.getAttribute('aria-label')
      || el.getAttribute('title')
      || el.getAttribute('alt')
      || el.getAttribute('placeholder')
      || (el.tagName === 'IMG' ? (el.getAttribute('src') || '').split('/').pop() : null)
      || (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, MAX_NAME);
    return n || null;
  };
  const items = [];
  let nodeCount = 0, refN = 0;
  const walk = (el, depth) => {
    if (nodeCount > MAX_NODES || items.length >= MAX_ITEMS || depth > 24) return;
    for (const child of el.childNodes) {
      if (nodeCount > MAX_NODES || items.length >= MAX_ITEMS) break;
      if (child.nodeType === 3) {
        nodeCount++;
        const t = (child.textContent || '').trim().replace(/\s+/g, ' ');
        if (t) items.push({ d: depth, k: 'text', n: t.slice(0, MAX_TEXT) });
        continue;
      }
      if (child.nodeType !== 1) continue;
      const e = child;
      nodeCount++;
      if (SKIP.has(e.tagName)) continue;
      if (e.getAttribute('aria-hidden') === 'true') continue;
      if (e.type === 'hidden') continue;
      try {
        const cs = window.getComputedStyle(e);
        if (cs.display === 'none' || cs.visibility === 'hidden') continue;
      } catch (_) { /* detached mid-walk */ }
      const role = e.getAttribute('role') || implicitRole(e);
      if (role === 'presentation' || role === 'none') { walk(e, depth + 1); continue; }
      if (role === 'heading') {
        const m = e.tagName.match(/^H([1-6])$/);
        items.push({ d: depth, k: 'heading', n: nameOf(e), l: m ? m[1] : '' });
        walk(e, depth + 1);
        continue;
      }
      if (role && isInteractive(e, role)) {
        const item = { d: depth, k: 'el', r: role, n: nameOf(e) };
        if (e.tagName === 'A') item.h = (e.getAttribute('href') || '').slice(0, 100);
        if (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA' || e.tagName === 'SELECT') {
          const t = (e.getAttribute('type') || '').toLowerCase();
          if (t) item.t = t;
          if (t === 'password') item.v = '***';
          else if (e.value !== undefined && e.value !== null && String(e.value) !== '') {
            item.v = String(e.value).slice(0, 80);
          }
        }
        refN++;
        const ref = 'e' + refN;
        item.ref = ref;
        refs.set(ref, e);
        items.push(item);
        if (e.tagName === 'SELECT' || e.tagName === 'OPTION') continue;
        walk(e, depth + 1);
        continue;
      }
      if (role && STRUCT_ROLES.has(role)) {
        items.push({ d: depth, k: 'el', r: role, n: nameOf(e) });
      }
      walk(e, depth + 1);
    }
  };
  const body = document.body || document.documentElement;
  if (body) walk(body, 0);
  return JSON.stringify({
    url: location.href,
    title: document.title || '',
    ready: document.readyState,
    items,
    refs: refN,
    truncated: nodeCount > MAX_NODES || items.length >= MAX_ITEMS
  });
})()
"#
}

/// Click the element a snapshot assigned `ref` to. Synthetic pointer event
/// sequence + `el.click()` covers both React-onclick handlers and plain
/// links. Returns `{ok}` or `{ok:false,error:'stale_ref'}`.
pub fn click_js(r#ref: &str) -> String {
    let r = serde_json::to_string(r#ref).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"((refId) => {{
  const el = window.__sr_refs && window.__sr_refs.get(refId);
  if (!el || !el.isConnected) return JSON.stringify({{ ok: false, error: 'stale_ref' }});
  try {{ el.scrollIntoView({{ block: 'center', inline: 'nearest' }}); }} catch (_) {{}}
  const r = el.getBoundingClientRect();
  const opts = {{ bubbles: true, cancelable: true, view: window, clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 }};
  try {{
    el.dispatchEvent(new PointerEvent('pointerdown', opts));
    el.dispatchEvent(new MouseEvent('mousedown', opts));
    el.dispatchEvent(new PointerEvent('pointerup', opts));
    el.dispatchEvent(new MouseEvent('mouseup', opts));
  }} catch (_) {{}}
  el.click();
  return JSON.stringify({{ ok: true }});
}})({r})"#
    )
}

/// Set text on an input/textarea/contenteditable assigned `ref`, using the
/// native value setter so framework state (React's controlled inputs)
/// notices; `submit` triggers the owning form when present. Returns
/// `{ok}` / `{ok:false,error:'stale_ref'}` / `{ok:false,error:'not_typable'}`.
pub fn type_js(r#ref: &str, text: &str, submit: bool) -> String {
    let r = serde_json::to_string(r#ref).unwrap_or_else(|_| "\"\"".into());
    let t = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let s = if submit { "true" } else { "false" };
    format!(
        r#"((refId, text, submit) => {{
  const el = window.__sr_refs && window.__sr_refs.get(refId);
  if (!el || !el.isConnected) return JSON.stringify({{ ok: false, error: 'stale_ref' }});
  try {{ el.scrollIntoView({{ block: 'center' }}); }} catch (_) {{}}
  el.focus();
  if (el.isContentEditable) {{
    el.textContent = text;
    el.dispatchEvent(new InputEvent('input', {{ bubbles: true }}));
  }} else if (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT') {{
    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
    setter.call(el, text);
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  }} else {{
    return JSON.stringify({{ ok: false, error: 'not_typable' }});
  }}
  if (submit) {{
    if (el.form && typeof el.form.requestSubmit === 'function') el.form.requestSubmit();
    else el.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }}));
  }}
  return JSON.stringify({{ ok: true }});
}})({r}, {t}, {s})"#
    )
}

/// Evaluate an arbitrary expression (via `eval`, so both expressions and
/// statement bodies work) and return `{ok,value}` / `{ok:false,error}` as a
/// JSON string. `undefined` maps to null; non-serializable values report
/// their toString.
pub fn eval_js(expression: &str) -> String {
    let src = serde_json::to_string(expression).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"((src) => {{
  try {{
    const r = (0, eval)(src);
    let value;
    if (r === undefined || r === null) value = null;
    else if (typeof r === 'bigint') value = String(r);
    else {{ try {{ JSON.stringify(r); value = r; }} catch (_) {{ value = String(r); }} }}
    return JSON.stringify({{ ok: true, value }});
  }} catch (e) {{
    return JSON.stringify({{ ok: false, error: String(e && e.message || e) }});
  }}
}})({src})"#
    )
}

/// Read `location.href`, `document.title` and `document.readyState` in one
/// round trip (navigation settle polling).
pub fn page_state_js() -> &'static str {
    r#"JSON.stringify({ url: location.href, title: document.title || '', ready: document.readyState })"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_scripts_embed_json_escaped_arguments() {
        // A hostile ref that tries to break out of its JS string literal
        // stays safely JSON-escaped inside the generated argument.
        let hostile = "e1\"; drop(); (";
        let click = click_js(hostile);
        assert!(click.contains(r#""e1\"; drop(); (""#), "{click}");
        let typed = type_js("e2", "line1\nline2 \"quoted\"", false);
        assert!(typed.contains("refId"), "{typed}");
        assert!(typed.contains("line1\\nline2"), "{typed}");
        let ev = eval_js("(() => 1)()");
        assert!(ev.contains("\"(() => 1)()\""), "{ev}");
    }
    #[test]
    fn snapshot_script_is_an_iife_returning_json_string() {
        let js = snapshot_js();
        assert!(js.trim_start().starts_with("(() => {"));
        assert!(js.trim_end().ends_with("})()"));
        assert!(js.contains("window.__sr_refs"));
    }
}
