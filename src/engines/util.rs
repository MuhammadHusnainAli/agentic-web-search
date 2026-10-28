//! Parsing helpers shared by engine adapters.

use scraper::{ElementRef, Selector};
use url::Url;

/// Compile a selector known to be valid at build time.
pub(crate) fn sel(css: &str) -> Selector {
    Selector::parse(css).unwrap_or_else(|e| panic!("invalid selector `{css}`: {e}"))
}

/// Visible text of an element with whitespace collapsed to single spaces.
pub(crate) fn text(el: ElementRef<'_>) -> String {
    let mut out = String::new();
    for chunk in el.text() {
        for word in chunk.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out
}

/// Text of the first element matching any of `selectors` (in priority order) that is non-empty.
pub(crate) fn first_text(root: ElementRef<'_>, selectors: &[Selector]) -> String {
    selectors
        .iter()
        .flat_map(|s| root.select(s))
        .map(text)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// Resolve `href` against `base`, keeping only absolute http(s) URLs.
pub(crate) fn absolute_http(href: &str, base: &Url) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    let url = base.join(href).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// Host without a leading `www.`.
pub fn domain_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_owned()))
        .unwrap_or_default()
}

/// Case-insensitive check for any of the given ASCII markers in `haystack`.
pub(crate) fn contains_any_ci(haystack: &str, markers: &[&str]) -> bool {
    // SERPs are large; avoid allocating a lowercase copy for every check by
    // scanning windows with ASCII case-insensitive comparison.
    let h = haystack.as_bytes();
    markers.iter().any(|m| {
        let m = m.as_bytes();
        m.len() <= h.len() && h.windows(m.len()).any(|w| w.eq_ignore_ascii_case(m))
    })
}

/// Generic block signals: rate limiting / forbidden status codes.
pub(crate) fn blocked_status(status: u16) -> bool {
    matches!(status, 403 | 429 | 503)
}

/// Contents of the first `<title>` element, if any (no allocation).
pub(crate) fn html_title(html: &str) -> &str {
    let bytes = html.as_bytes();
    let Some(open) = bytes.windows(6).position(|w| w.eq_ignore_ascii_case(b"<title")) else {
        return "";
    };
    let Some(gt) = html[open..].find('>') else {
        return "";
    };
    let start = open + gt + 1;
    let end = html[start..].find("</").map_or(html.len(), |e| start + e);
    html[start..end].trim()
}

/// Generic bot-wall detection shared by all engines: blocked status codes or a
/// challenge-style `<title>`.
pub(crate) fn generic_block(status: u16, html: &str) -> bool {
    blocked_status(status)
        || contains_any_ci(
            html_title(html),
            &[
                "captcha",
                "challenge",
                "verify you are human",
                "just a moment",
                "access denied",
                "are you a robot",
            ],
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_strips_www() {
        assert_eq!(domain_of("https://www.rust-lang.org/learn"), "rust-lang.org");
        assert_eq!(domain_of("http://docs.rs"), "docs.rs");
        assert_eq!(domain_of("not a url"), "");
    }

    #[test]
    fn absolute_http_filters_schemes() {
        let base = Url::parse("https://example.com/search").unwrap();
        assert_eq!(
            absolute_http("/a?b=1", &base).unwrap().as_str(),
            "https://example.com/a?b=1"
        );
        assert!(absolute_http("javascript:void(0)", &base).is_none());
        assert!(absolute_http("mailto:x@y.z", &base).is_none());
        assert!(absolute_http("#top", &base).is_none());
    }

    #[test]
    fn extracts_title() {
        assert_eq!(html_title("<html><head><TITLE lang=en> Captcha </title>"), "Captcha");
        assert_eq!(html_title("<html>no title"), "");
        assert!(generic_block(200, "<title>Just a moment...</title>"));
        assert!(generic_block(429, ""));
        assert!(!generic_block(200, "<title>rust - Brave Search</title> challenge"));
    }

    #[test]
    fn marker_search_is_case_insensitive() {
        assert!(contains_any_ci("<title>CAPTCHA</title>", &["captcha"]));
        assert!(!contains_any_ci("hello", &["captcha", "longer-than-haystack"]));
    }
}
