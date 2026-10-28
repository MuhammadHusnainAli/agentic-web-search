//! Readable-text extraction from arbitrary HTML pages.

use std::sync::LazyLock;

use ego_tree::iter::Edge;
use scraper::{Html, Node, Selector};

use crate::engines::util::sel;

static ROOTS: LazyLock<[Selector; 4]> =
    LazyLock::new(|| [sel("article"), sel("main"), sel("[role='main']"), sel("body")]);

/// Subtrees that never contain primary content.
const SKIP: &[&str] = &[
    "script", "style", "noscript", "svg", "nav", "footer", "header", "aside", "form", "iframe", "template", "button",
    "select", "canvas", "video", "audio", "picture", "head",
];

/// Elements after which a line break is emitted.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "br",
    "tr",
    "table",
    "section",
    "article",
    "blockquote",
    "pre",
    "dd",
    "dt",
    "figcaption",
];

/// Extract the main readable text of `html`, at most `max_chars` characters.
pub fn extract_text(html: &str, max_chars: usize) -> String {
    let doc = Html::parse_document(html);
    let root = ROOTS
        .iter()
        .find_map(|s| doc.select(s).next())
        .unwrap_or_else(|| doc.root_element());

    let mut out = String::with_capacity(max_chars.min(16 * 1024));
    let mut skip_depth = 0usize;
    let mut chars = 0usize;
    let mut line_has_text = false;

    for edge in root.traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(el) if skip_depth > 0 || SKIP.contains(&el.name()) => skip_depth += 1,
                Node::Text(t) if skip_depth == 0 => {
                    for word in t.split_whitespace() {
                        let need = word.chars().count() + usize::from(line_has_text);
                        if chars + need > max_chars {
                            return finish(out);
                        }
                        if line_has_text {
                            out.push(' ');
                        }
                        out.push_str(word);
                        chars += need;
                        line_has_text = true;
                    }
                }
                _ => {}
            },
            Edge::Close(node) => {
                if let Node::Element(el) = node.value() {
                    if skip_depth > 0 {
                        skip_depth -= 1;
                    } else if line_has_text && BLOCK.contains(&el.name()) {
                        out.push('\n');
                        chars += 1;
                        line_has_text = false;
                    }
                }
            }
        }
    }
    finish(out)
}

fn finish(mut s: String) -> String {
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_main_content_and_skips_chrome() {
        let html = r#"<html><head><title>t</title><style>.x{}</style></head><body>
            <nav>Home About</nav>
            <article><h1>Tokio  guide</h1><p>Async <b>runtime</b> for Rust.</p>
            <script>var x = 1;</script><p>Second paragraph.</p></article>
            <footer>Copyright</footer></body></html>"#;
        assert_eq!(
            extract_text(html, 1000),
            "Tokio guide\nAsync runtime for Rust.\nSecond paragraph."
        );
    }

    #[test]
    fn respects_max_chars() {
        let html = "<body><p>one two three four five</p></body>";
        let out = extract_text(html, 9);
        assert_eq!(out, "one two");
        assert!(out.chars().count() <= 9);
    }

    #[test]
    fn handles_unicode_limits() {
        let html = "<body><p>héllo wörld ünïcode</p></body>";
        assert_eq!(extract_text(html, 11), "héllo wörld");
    }
}
