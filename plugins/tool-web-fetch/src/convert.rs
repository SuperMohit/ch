//! Content classification and HTML conversion.

use ch_kernel::util::looks_binary;
use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};

/// Tags whose content is noise for a reader of the page.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "nav", "svg", "iframe", "template", "canvas", "object", "embed",
    "form", "button", "select",
];

/// What a response body is, for deciding how to present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Html,
    Json,
    Text,
    Binary,
}

/// Classifies a body from its `Content-Type` (if any) and its first bytes.
pub(crate) fn classify(content_type: Option<&str>, body: &[u8]) -> Kind {
    if looks_binary(body) {
        return Kind::Binary;
    }
    let Some(ct) = content_type else {
        return sniff(body);
    };
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "" => sniff(body),
        "text/html" | "application/xhtml+xml" => Kind::Html,
        "application/json" | "text/json" => Kind::Json,
        e if e.ends_with("+json") => Kind::Json,
        e if e.starts_with("text/") => Kind::Text,
        e if e.ends_with("+xml") => Kind::Text,
        "application/xml"
        | "application/javascript"
        | "application/x-javascript"
        | "application/ecmascript"
        | "application/yaml"
        | "application/x-yaml"
        | "application/toml"
        | "application/x-sh"
        | "application/sql"
        | "application/graphql"
        | "application/x-ndjson"
        | "application/x-www-form-urlencoded" => Kind::Text,
        _ => Kind::Binary,
    }
}

fn sniff(body: &[u8]) -> Kind {
    let head = String::from_utf8_lossy(&body[..body.len().min(512)]).to_ascii_lowercase();
    let head = head.trim_start();
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        Kind::Html
    } else if head.starts_with('{') || head.starts_with('[') {
        Kind::Json
    } else {
        Kind::Text
    }
}

/// HTML → markdown, dropping scripts, styles, navigation and form controls.
pub(crate) fn html_to_markdown(html: &str) -> String {
    let converter = HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build();
    tidy(&converter.convert(html).unwrap_or_else(|_| html.to_string()))
}

/// HTML → plain text: like markdown, but links, images, emphasis and code spans keep
/// only their text.
pub(crate) fn html_to_text(html: &str) -> String {
    fn inner(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
        Some(handlers.walk_children(element.node).content.into())
    }
    fn image(_: &dyn Handlers, element: Element) -> Option<HandlerResult> {
        element
            .attrs
            .iter()
            .find(|a| &*a.name.local == "alt")
            .map(|a| a.value.trim().to_string())
            .filter(|alt| !alt.is_empty())
            .map(Into::into)
    }
    fn heading(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
        let text = handlers.walk_children(element.node).content;
        let text = text.trim();
        (!text.is_empty()).then(|| format!("\n\n{text}\n\n").into())
    }
    let converter = HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .add_handler(
            vec![
                "a", "strong", "b", "em", "i", "u", "s", "del", "code", "span", "mark",
            ],
            inner,
        )
        .add_handler(vec!["img"], image)
        .add_handler(vec!["h1", "h2", "h3", "h4", "h5", "h6"], heading)
        .build();
    tidy(&converter.convert(html).unwrap_or_else(|_| html.to_string()))
}

/// Trims trailing spaces and collapses runs of blank lines.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// A short single-line excerpt of an error body, for HTTP error messages.
pub(crate) fn excerpt(content_type: Option<&str>, body: &[u8], max_chars: usize) -> String {
    let text = match classify(content_type, body) {
        Kind::Binary => return String::new(),
        Kind::Html => html_to_text(&String::from_utf8_lossy(body)),
        Kind::Json | Kind::Text => String::from_utf8_lossy(body).into_owned(),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars {
        let cut: String = flat.chars().take(max_chars).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(
            classify(Some("text/html; charset=utf-8"), b"<p>"),
            Kind::Html
        );
        assert_eq!(classify(Some("application/json"), b"{}"), Kind::Json);
        assert_eq!(
            classify(Some("application/vnd.api+json"), b"{}"),
            Kind::Json
        );
        assert_eq!(classify(Some("text/plain"), b"hi"), Kind::Text);
        assert_eq!(classify(Some("application/rss+xml"), b"<rss>"), Kind::Text);
        assert_eq!(classify(Some("image/png"), b"PNG"), Kind::Binary);
        assert_eq!(classify(Some("application/pdf"), b"%PDF"), Kind::Binary);
        assert_eq!(classify(Some("text/plain"), b"a\0b"), Kind::Binary);
        assert_eq!(classify(None, b"<!DOCTYPE html><html>"), Kind::Html);
        assert_eq!(classify(None, b"[1,2]"), Kind::Json);
        assert_eq!(classify(None, b"hello"), Kind::Text);
    }

    #[test]
    fn markdown_drops_noise() {
        let html = r#"<html><head><style>p{}</style><script>alert(1)</script></head>
            <body><nav><a href="/">Home</a></nav><h1>Title</h1>
            <p>Some <strong>bold</strong> and <a href="https://x.dev">a link</a>.</p></body></html>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("**bold**"), "{md}");
        assert!(md.contains("[a link](https://x.dev)"), "{md}");
        assert!(!md.contains("alert"), "{md}");
        assert!(!md.contains("Home"), "{md}");
        assert!(!md.contains("p{}"), "{md}");
    }

    #[test]
    fn text_strips_markup() {
        let html = r#"<h2>Intro</h2><p>Some <em>stuff</em> and <a href="https://x.dev">a link</a>
            <img src="a.png" alt="diagram"></p>"#;
        let text = html_to_text(html);
        assert!(text.starts_with("Intro"), "{text}");
        assert!(text.contains("Some stuff and a link"), "{text}");
        assert!(text.contains("diagram"), "{text}");
        assert!(
            !text.contains('#') && !text.contains("](") && !text.contains('*'),
            "{text}"
        );
    }

    #[test]
    fn excerpts_are_short_and_flat() {
        let e = excerpt(Some("text/html"), b"<h1>Not   Found</h1>\n<p>Nope</p>", 100);
        assert_eq!(e, "Not Found Nope");
        let long = "x".repeat(500);
        assert_eq!(
            excerpt(Some("text/plain"), long.as_bytes(), 10)
                .chars()
                .count(),
            11
        );
        assert_eq!(excerpt(Some("image/png"), b"png", 10), "");
    }
}
