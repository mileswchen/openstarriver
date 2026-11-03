//! Pure snapshot rendering: the page-side walker (see [`crate::scripts`])
//! returns a JSON item list; this module turns it into the model-facing
//! text outline. No I/O, no browser — fully unit-testable.
//!
//! Item shapes the walker emits (compact keys to keep the evaluate payload
//! small):
//! - `{"k":"text","d":3,"n":"Some text"}`
//! - `{"k":"heading","d":1,"n":"Products","l":"2"}`
//! - `{"k":"el","d":2,"r":"link","n":"Docs","ref":"e12","h":"/docs"}`
//!   optional extras: `v` (current value), `t` (input type).

/// Footer appended when the char cap cuts a rendered snapshot.
pub const SNAPSHOT_TRUNCATION_FOOTER: &str =
    "\n(snapshot truncated — narrow the scope with browser_eval or navigate to a more specific page)";

/// Render one item as a `-role "name"` line piece (no indent).
fn item_line(item: &serde_json::Value) -> Option<String> {
    let kind = item.get("k")?.as_str()?;
    match kind {
        "text" => {
            let n = item.get("n").and_then(|v| v.as_str()).unwrap_or("");
            Some(n.to_string())
        }
        "heading" => {
            let n = item.get("n").and_then(|v| v.as_str()).unwrap_or("");
            let l = item.get("l").and_then(|v| v.as_str()).unwrap_or("");
            Some(format!("heading {l} \"{n}\""))
        }
        "el" => {
            let role = item.get("r").and_then(|v| v.as_str()).unwrap_or("element");
            let name = item.get("n").and_then(|v| v.as_str()).unwrap_or("");
            let mut line = String::new();
            if let Some(r#ref) = item.get("ref").and_then(|v| v.as_str()) {
                line.push_str(&format!("[{ref_id}] ", ref_id = r#ref));
            }
            line.push_str(&format!("{role} \"{name}\""));
            if let Some(t) = item.get("t").and_then(|v| v.as_str()) {
                line.push_str(&format!(" type={t}"));
            }
            if let Some(v) = item.get("v").and_then(|v| v.as_str()) {
                line.push_str(&format!(" value=\"{v}\""));
            }
            if let Some(h) = item.get("h").and_then(|v| v.as_str()) {
                line.push_str(&format!(" → {h}"));
            }
            Some(line)
        }
        _ => None,
    }
}

/// Render the walker payload into the text snapshot the model reads.
/// Header carries url/title/readyState; items indent by DOM depth (2 spaces,
/// capped so wide trees don't blow up the line prefix). Truncates on char
/// boundaries with [`SNAPSHOT_TRUNCATION_FOOTER`]; `max_chars` of 0 disables
/// the cap.
pub fn render_snapshot(payload: &serde_json::Value, max_chars: usize) -> String {
    let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = format!("url: {url}\ntitle: {title}\n");
    let items = match payload.get("items").and_then(|v| v.as_array()) {
        Some(items) if !items.is_empty() => items,
        _ => {
            out.push_str("(no content)");
            return out;
        }
    };
    let mut rendered_items = 0usize;
    for item in items {
        let Some(line) = item_line(item) else {
            continue;
        };
        let depth = item.get("d").and_then(|v| v.as_u64()).unwrap_or(0).min(16) as usize;
        let line = format!("{}{}\n", "  ".repeat(depth), line);
        if max_chars > 0 && out.chars().count() + line.chars().count() > max_chars {
            out.push_str(SNAPSHOT_TRUNCATION_FOOTER);
            return out;
        }
        out.push_str(&line);
        rendered_items += 1;
    }
    if payload
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        out.push_str(&format!(
            "\n(page outline truncated by the walker at {rendered_items} rendered items)"
        ));
    }
    out
}

/// Validate a navigation target. Only http/https is navigable (`about:blank`
/// stays the session's initial page but is not a valid explicit target);
/// file:// and every other scheme are refused, and control characters or a
/// >2 KiB URL are rejected. Returns the URL unchanged on success.
pub fn validate_navigate_url(url: &str) -> Result<&str, String> {
    let lowered = url.trim().to_ascii_lowercase();
    if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
        return Err(format!(
            "only http/https URLs are navigable (got `{url}`; file:// and other schemes are refused)"
        ));
    }
    if url.len() > 2048 {
        return Err("URL longer than 2048 characters".into());
    }
    if url.chars().any(|c| c.is_control()) {
        return Err("URL contains control characters".into());
    }
    Ok(url)
}

/// Parse a `WIDTHxHEIGHT` viewport spec like `1280x800`. Defaults to
/// `1280x800` for None/empty; invalid shapes error (never silently
/// misrender).
pub fn parse_viewport(spec: Option<&str>) -> Result<(u32, u32), String> {
    let Some(spec) = spec.filter(|s| !s.trim().is_empty()) else {
        return Ok((1280, 800));
    };
    let Some((w, h)) = spec.split_once(['x', 'X', '×']) else {
        return Err(format!("viewport must look like 1280x800 (got `{spec}`)"));
    };
    let w: u32 = w
        .trim()
        .parse()
        .map_err(|_| format!("viewport width not a number: `{w}`"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("viewport height not a number: `{h}`"))?;
    if !(320..=7680).contains(&w) || !(240..=4320).contains(&h) {
        return Err(format!("viewport {w}x{h} outside 320..7680 x 240..4320"));
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(items: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "url": "https://example.com/",
            "title": "Example",
            "ready": "complete",
            "items": items,
        })
    }

    #[test]
    fn renders_items_with_refs_and_indent() {
        let p = payload(serde_json::json!([
            { "k": "heading", "d": 0, "n": "Products", "l": "1" },
            { "k": "text", "d": 1, "n": "3 results found" },
            { "k": "el", "d": 1, "r": "link", "n": "Docs", "ref": "e1", "h": "/docs" },
            { "k": "el", "d": 2, "r": "textbox", "n": "Search", "ref": "e2", "t": "search", "v": "cats" },
        ]));
        let text = render_snapshot(&p, 0);
        assert!(text.contains("url: https://example.com/"));
        assert!(text.contains("title: Example"));
        assert!(text.contains("heading 1 \"Products\""));
        assert!(text.contains("  3 results found"));
        assert!(text.contains("  [e1] link \"Docs\" → /docs"));
        assert!(text.contains("    [e2] textbox \"Search\" type=search value=\"cats\""));
    }

    #[test]
    fn truncates_on_char_boundary_with_footer() {
        let many: Vec<_> = (0..500)
            .map(|i| serde_json::json!({ "k": "text", "d": 0, "n": format!("line-{i}") }))
            .collect();
        let text = render_snapshot(&payload(serde_json::Value::Array(many)), 120);
        assert!(text.contains("snapshot truncated"), "{text}");
        // The content prefix before the footer stays within the cap; the
        // footer itself is what pushes the total past it.
        let prefix = text.split(SNAPSHOT_TRUNCATION_FOOTER).next().unwrap();
        assert!(prefix.chars().count() <= 120, "{}", prefix.chars().count());
        assert!(text.chars().count() <= 120 + SNAPSHOT_TRUNCATION_FOOTER.chars().count() + 2);
    }

    #[test]
    fn walker_truncation_flag_is_surfaced() {
        let mut p = payload(serde_json::json!([
            { "k": "text", "d": 0, "n": "partial content" }
        ]));
        p["truncated"] = serde_json::json!(true);
        let text = render_snapshot(&p, 0);
        assert!(text.contains("partial content"), "{text}");
        assert!(text.contains("truncated by the walker"), "{text}");
    }

    #[test]
    fn empty_items_renders_placeholder() {
        let text = render_snapshot(&payload(serde_json::Value::Array(vec![])), 0);
        assert!(text.contains("(no content)"), "{text}");
    }

    #[test]
    fn url_gate_accepts_http_and_https_only() {
        assert!(validate_navigate_url("https://example.com").is_ok());
        assert!(validate_navigate_url("HTTP://EXAMPLE.COM").is_ok());
        assert!(validate_navigate_url("http://localhost:3000/x").is_ok());
        assert!(validate_navigate_url("file:///etc/passwd").is_err());
        assert!(validate_navigate_url("about:blank").is_err());
        assert!(validate_navigate_url("javascript:alert(1)").is_err());
        assert!(validate_navigate_url("").is_err());
        assert!(
            validate_navigate_url(&format!("https://example.com/{}", "a".repeat(2100))).is_err()
        );
        assert!(validate_navigate_url("https://example.com/\u{1}").is_err());
    }

    #[test]
    fn viewport_parsing_defaults_and_bounds() {
        assert_eq!(parse_viewport(None).unwrap(), (1280, 800));
        assert_eq!(parse_viewport(Some("")).unwrap(), (1280, 800));
        assert_eq!(parse_viewport(Some("1440x900")).unwrap(), (1440, 900));
        assert_eq!(parse_viewport(Some("800X600")).unwrap(), (800, 600));
        assert!(parse_viewport(Some("tiny")).is_err());
        assert!(parse_viewport(Some("100x100")).is_err());
        assert!(parse_viewport(Some("ax800")).is_err());
    }
}
