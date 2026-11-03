//! Pure body-rendering pipeline for web_fetch: content-type classification,
//! charset decoding, HTML→markdown conversion with a depth guard, and a
//! output cap. No I/O, no network, no HTTP.

use std::sync::{Arc, OnceLock};

use htmd::HtmlToMarkdown;

use super::web_common::clean_html;

/// How a fetched body renders to model-facing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// Converted through the HTML→markdown pipeline.
    Html,
    /// Passed through verbatim (after charset decoding).
    Text,
}

/// Classify a response by its Content-Type header. `text/html` and
/// `application/xhtml+xml` render as HTML; other `text/*` plus a few
/// structured text types pass through; unknown or missing types are sniffed
/// (a leading `<` after whitespace counts as HTML) and never error.
pub fn classify_content_type(content_type: Option<&str>, body: &[u8]) -> BodyKind {
    let media = content_type
        .map(|ct| ct.split(';').next().unwrap_or_default().trim())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if media == "text/html" || media == "application/xhtml+xml" {
        return BodyKind::Html;
    }
    if media.starts_with("text/")
        || media == "application/json"
        || media == "application/xml"
        || media.ends_with("+json")
        || media.ends_with("+xml")
    {
        return BodyKind::Text;
    }
    // Unknown or missing Content-Type: sniff. A leading '<' (after
    // whitespace) is treated as HTML; anything else renders as text. Skip a
    // UTF-8 BOM first (HTML5's own sniffing algorithm does) — a BOM-prefixed
    // HTML page classified as Text dumps raw markup.
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    match body.iter().copied().find(|b| !b.is_ascii_whitespace()) {
        Some(b'<') => BodyKind::Html,
        _ => BodyKind::Text,
    }
}

/// Extract the `charset` parameter from a Content-Type header, or `None` when
/// absent, empty, or the header carries no parameters. The value is returned
/// as-is; `encoding_rs::Encoding::for_label` matches labels case-insensitively.
pub fn parse_charset(content_type: Option<&str>) -> Option<&str> {
    let params = content_type?.split_once(';')?.1;
    for part in params.split(';') {
        if let Some((key, value)) = part.trim().split_once('=') {
            if key.trim().eq_ignore_ascii_case("charset") {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Decode a raw body with the declared charset label, falling back to UTF-8
/// lossy when the label is missing or unknown (never an error).
pub fn decode_body(body: &[u8], charset: Option<&str>) -> String {
    match charset.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())) {
        Some(encoding) => encoding.decode(body).0.into_owned(),
        None => String::from_utf8_lossy(body).into_owned(),
    }
}

/// How many leading body bytes are scanned for an HTML charset declaration
/// (HTML5: the encoding declaration must appear within the first 1024 bytes
/// of the document to take effect).
pub const META_CHARSET_SCAN_BYTES: usize = 1024;

/// Scan the head of an HTML document for a charset declaration:
/// `<meta charset="gbk">` or
/// `<meta http-equiv="Content-Type" content="text/html; charset=gbk">`
/// (case-insensitive; both reduce to a `charset=` value inside a `<meta>` tag).
/// The prefix is lossily decoded first — every real-world encoding is a
/// ASCII-compatible superset for the ASCII bytes of the declaration itself,
/// so the label survives the lossy pass.
pub fn sniff_meta_charset(prefix: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(prefix);
    let re_meta = regex_lite::Regex::new(r"(?is)<meta\b[^>]*>").expect("static regex");
    let re_charset = regex_lite::Regex::new(r#"(?i)charset\s*=\s*["']?\s*([a-z0-9_.:\-]+)"#)
        .expect("static regex");
    let re_equiv =
        regex_lite::Regex::new(r#"(?i)http-equiv\s*=\s*["']?content-type"#).expect("static regex");
    // charset as a standalone ATTRIBUTE: the match must sit OUTSIDE any
    // quoted attribute value — `content="0; charset=gbk"` (or a refresh URL
    // carrying `charset=`) also satisfies `\scharset\s*=` and hijacked the
    // decoder. Scan quotes before the match: an even count means we are
    // between attributes, odd means inside a value.
    let re_plain =
        regex_lite::Regex::new(r#"(?i)^<meta\b[^>]*?(\s)charset\s*="#).expect("static regex");
    for tag in re_meta.captures_iter(&text) {
        // BUG-95: only the two sanctioned declarations count — matching
        // `charset=` anywhere in ANY meta tag let a refresh URL like
        // `url=/?charset=gbk` hijack the decoder.
        let mut sanctioned = re_equiv.is_match(&tag[0]);
        if !sanctioned {
            if let Some(m) = re_plain.captures(&tag[0]) {
                // Count quote characters BEFORE the whitespace that precedes
                // `charset=` — even = the match is at attribute position.
                let head = &tag[0][..m.get(1).map(|g| g.start()).unwrap_or(0)];
                let quotes = head.matches(['"', '\'']).count();
                sanctioned = quotes % 2 == 0;
            }
        }
        if !sanctioned {
            continue;
        }
        if let Some(value) = re_charset.captures(&tag[0]) {
            let label = value[1].to_string();
            // Only trust labels that name a real encoding.
            if encoding_rs::Encoding::for_label(label.as_bytes()).is_some() {
                return Some(label);
            }
        }
    }
    None
}

/// Effective decode charset for a response body, in priority order: HTTP
/// `Content-Type` header charset, then (HTML bodies only) a scanned
/// `<meta>` declaration within [`META_CHARSET_SCAN_BYTES`], then `None`
/// (decode as UTF-8 with replacement — never an error).
pub fn resolve_charset(content_type: Option<&str>, body: &[u8], kind: BodyKind) -> Option<String> {
    if let Some(label) = parse_charset(content_type) {
        return Some(label.to_string());
    }
    if kind == BodyKind::Html {
        let scan_end = body.len().min(META_CHARSET_SCAN_BYTES);
        return sniff_meta_charset(&body[..scan_end]);
    }
    None
}

/// Maximum HTML nesting accepted by the markdown converter. htmd's tree walk
/// recurses per element; the lexical pre-check in [`render_html`] keeps
/// adversarial nesting from overflowing the stack (uses the same 512
/// ceiling). A robustness invariant, not a tunable.
pub const MAX_HTML_DEPTH: usize = 512;

/// Elements that never take a closing tag, so they do not grow the lexical
/// stack.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose contents HTML parses as text until their matching end tag.
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "noscript"];

/// Whether a byte can occur after a raw-text end-tag name.
fn is_tag_boundary(b: u8) -> bool {
    b == b'>' || b == b'/' || b.is_ascii_whitespace()
}

/// First occurrence of `needle` at or after `from`, if any.
fn find_sequence(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// Find the matching raw-text end tag without interpreting markup-like body
/// text (`findRawTextEnd`).
fn find_raw_text_end(lower: &[u8], name: &[u8], from: usize) -> Option<usize> {
    let mut prefix = Vec::with_capacity(name.len() + 2);
    prefix.extend_from_slice(b"</");
    prefix.extend_from_slice(name);
    let mut from = from;
    loop {
        let candidate = find_sequence(lower, from, &prefix)?;
        if is_tag_boundary(*lower.get(candidate + prefix.len()).unwrap_or(&b'>')) {
            return Some(candidate);
        }
        from = candidate + 1;
    }
}

/// Conservatively reject HTML whose lexical element stack crosses the depth
/// ceiling. The single pass ignores raw-text bodies, respects quoted `>`
/// characters, and only accepts a closing tag for the current element;
/// malformed input (including markup inside comments) therefore over-counts
/// rather than hiding nesting (`exceedsConversionDepth`).
pub fn exceeds_max_depth(html: &str, max_depth: usize) -> bool {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut open: Vec<String> = Vec::new();
    let mut offset = 0usize;
    let mut in_comment = false;

    while offset < bytes.len() {
        let start = bytes[offset..]
            .iter()
            .position(|&b| b == b'<')
            .map(|i| i + offset);
        if in_comment {
            if let Some(end) = find_sequence(bytes, offset, b"-->") {
                match start {
                    // A `<` before the comment terminator keeps us inside.
                    Some(s) if s < end => {}
                    _ => {
                        in_comment = false;
                        offset = end + 3;
                        continue;
                    }
                }
            }
        }
        let Some(start) = start else { break };
        if !in_comment && bytes[start..].starts_with(b"<!--") {
            in_comment = true;
            offset = start + 4;
            continue;
        }

        let mut cursor = start + 1;
        let closing = bytes.get(cursor) == Some(&b'/');
        if closing {
            cursor += 1;
        }
        let name_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
        {
            cursor += 1;
        }
        if cursor == name_start
            || !bytes
                .get(name_start)
                .is_some_and(|b| b.is_ascii_alphabetic())
        {
            offset = start + 1;
            continue;
        }
        let name = &lower[name_start..cursor];

        // Consume the tag, honoring quoted `>` characters.
        let mut quote: Option<u8> = None;
        let mut terminated = false;
        while cursor < bytes.len() {
            let ch = bytes[cursor];
            cursor += 1;
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
            } else if ch == b'"' || ch == b'\'' {
                quote = Some(ch);
            } else if ch == b'>' {
                terminated = true;
                break;
            }
        }
        if !terminated {
            break;
        }

        if closing {
            if !in_comment && open.last().map(String::as_str) == Some(name) {
                open.pop();
            }
        } else {
            // Self-closing check: the last NON-BLANK byte before the `>` is
            // a `/`. An unquoted attribute value that ENDS with `/` (e.g.
            // `<i a=/>` — HTML5 treats `a=/` as the value, tag is open)
            // used to read the `/` as a self-close marker and skip the
            // depth push entirely.
            let mut last = cursor.saturating_sub(2);
            while last > 0 && bytes[last].is_ascii_whitespace() {
                last = last.saturating_sub(1);
            }
            // HTML5 ignores the trailing `/` on non-void HTML start tags
            // (only void + foreign elements self-close). Treating
            // `<div />`/`<span attr="x"/>` as self-closing UNDER-counted
            // nesting — the guard this function exists to be must
            // over-count, never hide depth.
            let _ = last;
            let self_closing = false;
            if !self_closing && !VOID_ELEMENTS.contains(&name) {
                open.push(name.to_string());
                if open.len() > max_depth {
                    return true;
                }
                if !in_comment && RAW_TEXT_ELEMENTS.contains(&name) {
                    match find_raw_text_end(bytes, name.as_bytes(), cursor) {
                        Some(end) => {
                            offset = end;
                            continue;
                        }
                        None => break,
                    }
                }
            }
        }
        offset = cursor;
    }
    false
}

/// HTML→markdown conversion, wrapped so tests can inject a failing renderer.
pub type MarkdownRenderer = dyn Fn(&str) -> Result<String, std::io::Error> + Send + Sync;

fn htmd_convert(html: &str) -> Result<String, std::io::Error> {
    static CONVERTER: OnceLock<HtmlToMarkdown> = OnceLock::new();
    let converter = CONVERTER.get_or_init(|| {
        HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "noscript"])
            .build()
    });
    converter.convert(html)
}

/// The default renderer: htmd with turndown-aligned options and explicit
/// `script/style/noscript` skipping (htmd does not remove them by default).
pub fn default_markdown_renderer() -> Arc<MarkdownRenderer> {
    Arc::new(htmd_convert)
}

/// Render a decoded HTML body to markdown. Deeply nested HTML (lexical depth
/// beyond [`MAX_HTML_DEPTH`]) and renderer failures degrade to the existing
/// plain-text cleaner; a degraded page beats an error (behavior).
pub fn render_html(html: &str, renderer: &MarkdownRenderer) -> String {
    if exceeds_max_depth(html, MAX_HTML_DEPTH) {
        return clean_html(html);
    }
    match renderer(html) {
        Ok(markdown) => markdown,
        Err(_) => clean_html(html),
    }
}

/// Footer appended whenever the 2 MiB byte cap or the configured output char
/// cap cut the response (wording).
pub const TRUNCATION_FOOTER: &str =
    "\n\n(Content truncated. Fetch a more specific URL or section for the full text.)";

/// Cap a rendered output prefix at `max_chars` characters (
/// `computeFetchOutput` semantics). When the byte cap already cut the source
/// (`force_footer`), the footer is appended even if the prefix fits; when the
/// prefix itself exceeds the cap it is cut so the footer survives; when the
/// cap is smaller than the footer itself, the hard cap wins and slices the
/// complete output (possibly cutting the footer). All cuts land on char
/// boundaries. Returns the final text and whether any truncation applies.
pub fn truncate_output(prefix: &str, max_chars: usize, force_footer: bool) -> (String, bool) {
    let truncated = force_footer || prefix.chars().count() > max_chars;
    if !truncated {
        return (prefix.to_string(), false);
    }
    let full = format!("{prefix}{TRUNCATION_FOOTER}");
    if full.chars().count() <= max_chars {
        return (full, true);
    }
    if max_chars < TRUNCATION_FOOTER.chars().count() {
        return (full.chars().take(max_chars).collect(), true);
    }
    let prefix_cut = max_chars - TRUNCATION_FOOTER.chars().count();
    let head: String = prefix.chars().take(prefix_cut).collect();
    (format!("{head}{TRUNCATION_FOOTER}"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_html_content_types() {
        assert_eq!(
            classify_content_type(Some("text/html; charset=utf-8"), b"<p>x</p>"),
            BodyKind::Html
        );
        assert_eq!(
            classify_content_type(Some("TEXT/HTML"), b"<p>x</p>"),
            BodyKind::Html
        );
        assert_eq!(
            classify_content_type(Some("application/xhtml+xml; charset=utf-8"), b"<p>x</p>"),
            BodyKind::Html
        );
    }

    #[test]
    fn classifies_plain_text_content_types() {
        assert_eq!(
            classify_content_type(Some("text/plain; charset=utf-8"), b"hello"),
            BodyKind::Text
        );
        assert_eq!(
            classify_content_type(Some("application/json"), b"{\"a\":1}"),
            BodyKind::Text
        );
        assert_eq!(
            classify_content_type(Some("application/ld+json"), b"{}"),
            BodyKind::Text
        );
        assert_eq!(
            classify_content_type(Some("application/xml"), b"<a/>"),
            BodyKind::Text
        );
        assert_eq!(
            classify_content_type(Some("application/rss+xml"), b"<rss/>"),
            BodyKind::Text
        );
    }

    #[test]
    fn sniffs_unknown_content_types() {
        assert_eq!(
            classify_content_type(None, b"<html><body>x</body></html>"),
            BodyKind::Html
        );
        assert_eq!(
            classify_content_type(Some("application/octet-stream"), b"\n  <div>x</div>"),
            BodyKind::Html
        );
        assert_eq!(
            classify_content_type(Some("application/octet-stream"), b"\n  hello"),
            BodyKind::Text
        );
        assert_eq!(classify_content_type(None, b"\x00\x01\x02"), BodyKind::Text);
    }

    #[test]
    fn parses_charset_param() {
        assert_eq!(
            parse_charset(Some("text/html; charset=utf-8")),
            Some("utf-8")
        );
        assert_eq!(
            parse_charset(Some("text/html; Charset=\"GBK\"")),
            Some("GBK")
        );
        assert_eq!(parse_charset(Some("text/html")), None);
        assert_eq!(parse_charset(Some("text/html; foo=bar")), None);
        assert_eq!(parse_charset(None), None);
    }

    #[test]
    fn decodes_declared_charset() {
        let gbk = encoding_rs::GBK.encode("你好").0;
        assert_eq!(decode_body(&gbk, Some("gbk")), "你好");
        assert_eq!(decode_body(b"hello", Some("utf-8")), "hello");
    }

    #[test]
    fn decodes_utf8_lossy_for_unknown_or_missing_charset() {
        assert_eq!(decode_body(b"hello", None), "hello");
        assert_eq!(decode_body(b"hello", Some("not-a-charset")), "hello");
        assert_eq!(
            decode_body(&[0xff, 0xfe], Some("not-a-charset")),
            "\u{FFFD}\u{FFFD}"
        );
    }

    #[test]
    fn sniffs_meta_charset_declarations() {
        assert_eq!(
            sniff_meta_charset(b"<html><head><meta charset=\"gbk\"></head>"),
            Some("gbk".to_string())
        );
        assert_eq!(
            sniff_meta_charset(
                b"<META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=GB2312\">"
            ),
            Some("GB2312".to_string())
        );
        assert_eq!(
            sniff_meta_charset(b"<meta charset='shift_jis'>"),
            Some("shift_jis".to_string())
        );
        assert_eq!(
            sniff_meta_charset(b"<meta name=\"author\" content=\"x\">"),
            None
        );
        assert_eq!(
            sniff_meta_charset(b"<meta http-equiv=\"refresh\" content=\"0; url=?charset=evil\">"),
            None
        );
        assert_eq!(sniff_meta_charset(b"no meta at all"), None);
    }

    #[test]
    fn sniff_ignores_declarations_beyond_scan_window() {
        let padding = vec![b' '; META_CHARSET_SCAN_BYTES];
        let mut html = padding;
        html.extend_from_slice(b"<meta charset=\"gbk\">");
        // The caller truncates to the window before scanning (resolve_charset
        // semantics): a declaration past the window is invisible.
        assert_eq!(sniff_meta_charset(&html[..META_CHARSET_SCAN_BYTES]), None);
        // A declaration fully inside the window is found even at the edge.
        let mut edge = vec![b' '; META_CHARSET_SCAN_BYTES - 24];
        edge.extend_from_slice(b"<meta charset=\"gbk\">");
        assert_eq!(sniff_meta_charset(&edge), Some("gbk".to_string()));
    }

    #[test]
    fn resolve_charset_prefers_header_over_meta_and_skips_text_bodies() {
        let meta_only = b"<meta charset=\"gbk\">";
        assert_eq!(
            resolve_charset(None, meta_only, BodyKind::Html),
            Some("gbk".to_string())
        );
        // Header wins even when it disagrees with the meta declaration.
        assert_eq!(
            resolve_charset(Some("text/html; charset=utf-8"), meta_only, BodyKind::Html),
            Some("utf-8".to_string())
        );
        // Non-HTML bodies never consult meta declarations.
        assert_eq!(resolve_charset(None, meta_only, BodyKind::Text), None);
        assert_eq!(resolve_charset(None, b"plain", BodyKind::Text), None);
    }

    #[test]
    fn decodes_gbk_body_declared_only_in_meta() {
        let mut html = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        html.extend_from_slice(&encoding_rs::GBK.encode("标题").0);
        let charset = resolve_charset(None, &html, BodyKind::Html);
        assert_eq!(charset.as_deref(), Some("gbk"));
        assert!(decode_body(&html, charset.as_deref()).contains("标题"));
    }

    #[test]
    fn depth_guard_accepts_shallow_nesting() {
        let html = "<div><section><p>hi</p></section></div>";
        assert!(!exceeds_max_depth(html, MAX_HTML_DEPTH));
        assert!(!exceeds_max_depth(html, 3));
    }

    #[test]
    fn depth_guard_rejects_deep_nesting() {
        let html = format!("{}<p>x</p>", "<div>".repeat(MAX_HTML_DEPTH + 1));
        assert!(exceeds_max_depth(&html, MAX_HTML_DEPTH));
    }

    #[test]
    fn depth_guard_skips_raw_text_bodies() {
        let html = "<div><script>var s = '<div><div><div>';</script>\
                    <style>.x { content: '<div>' }</style><p>x</p></div>";
        assert!(!exceeds_max_depth(html, 2));
    }

    #[test]
    fn depth_guard_over_counts_comment_markup() {
        // Markup inside comments is counted, never trusted to close: a
        // conservative over-count is safe for a DoS guard.
        assert!(exceeds_max_depth("<!-- <div> -->", 0));
        assert!(!exceeds_max_depth("<!-- <div> -->", 1));
    }

    #[test]
    fn depth_guard_counts_self_closing_and_void_elements() {
        let html = "<div><br><img src=\"x\"><input/><span>a</span></div>";
        assert!(!exceeds_max_depth(html, 2));
    }

    #[test]
    fn depth_guard_respects_quoted_gt() {
        let html = "<div title=\"a > b\"><p>x</p></div>";
        assert!(!exceeds_max_depth(html, 2));
    }

    #[test]
    fn renders_html_to_markdown() {
        let renderer = default_markdown_renderer();
        let html = "<h1>Title</h1><p>Hello <strong>world</strong> and \
                    <a href=\"https://x.com\">link</a>.</p>";
        let md = render_html(html, renderer.as_ref());
        assert!(md.contains("# Title"));
        assert!(md.contains("**world**"));
        assert!(md.contains("[link](https://x.com)"));
    }

    #[test]
    fn renders_tables_and_code_blocks() {
        let renderer = default_markdown_renderer();
        let html = "<table><thead><tr><th>A</th><th>B</th></tr></thead>\
                    <tbody><tr><td>1</td><td>2</td></tr></tbody></table>\
                    <pre><code>let x = 1;</code></pre>";
        let md = render_html(html, renderer.as_ref());
        assert!(md.contains("| A | B |"));
        assert!(md.contains("```"));
        assert!(md.contains("let x = 1;"));
    }

    #[test]
    fn skips_script_style_noscript() {
        let renderer = default_markdown_renderer();
        let html = "<p>a</p><script>alert('x')</script>\
                    <style>.y{}</style><noscript>ns</noscript><p>b</p>";
        let md = render_html(html, renderer.as_ref());
        assert!(!md.contains("alert"));
        assert!(!md.contains(".y"));
        assert!(!md.contains("ns"));
    }

    #[test]
    fn falls_back_to_clean_html_on_renderer_error() {
        let failing: Arc<MarkdownRenderer> =
            Arc::new(|_html: &str| Err(std::io::Error::other("boom")));
        let html = "<p>Hello <b>world</b></p>";
        assert_eq!(render_html(html, failing.as_ref()), clean_html(html));
    }

    #[test]
    fn falls_back_to_clean_html_on_excessive_depth() {
        let renderer = default_markdown_renderer();
        let html = format!("{}<p>x</p>", "<div>".repeat(MAX_HTML_DEPTH + 1));
        assert_eq!(render_html(&html, renderer.as_ref()), clean_html(&html));
    }

    #[test]
    fn truncation_passes_through_short_output() {
        let prefix = "HTTP 200\n\nhello";
        assert_eq!(
            truncate_output(prefix, 100, false),
            (prefix.to_string(), false)
        );
    }

    #[test]
    fn truncation_appends_footer_when_over_cap() {
        let prefix = "x".repeat(1000);
        let (text, truncated) = truncate_output(&prefix, 100, false);
        assert!(truncated);
        assert_eq!(text.chars().count(), 100);
        assert!(text.ends_with(TRUNCATION_FOOTER));
    }

    #[test]
    fn truncation_forces_footer_on_byte_cut() {
        let prefix = "HTTP 200\n\nhello";
        let (text, truncated) = truncate_output(prefix, 1000, true);
        assert!(truncated);
        assert!(text.ends_with(TRUNCATION_FOOTER));
    }

    #[test]
    fn truncation_hard_slices_when_cap_smaller_than_footer() {
        let prefix = "abcdef";
        let (text, truncated) = truncate_output(prefix, 5, false);
        assert!(truncated);
        assert_eq!(text, "abcde");
    }

    #[test]
    fn truncation_slices_on_char_boundary() {
        let prefix = "你好世界".repeat(25);
        let (text, truncated) = truncate_output(&prefix, 80, false);
        assert!(truncated);
        assert_eq!(text.chars().count(), 80);
        assert!(text.ends_with(TRUNCATION_FOOTER));
    }
}

// ── BUG-95 regression (round 6) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn refresh_url_charset_is_not_a_charset_declaration() {
        assert_eq!(
            sniff_meta_charset(b"<meta http-equiv=\"refresh\" content=\"0; url=/?charset=gbk\">"),
            None,
            "a real-looking label inside a refresh URL must not hijack the decoder"
        );
        // The sanctioned forms keep working.
        assert_eq!(
            sniff_meta_charset(b"<meta charset=\"utf-8\">"),
            Some("utf-8".into())
        );
        assert_eq!(
            sniff_meta_charset(
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\">"
            ),
            Some("gbk".into())
        );
    }
}
