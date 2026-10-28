use std::sync::LazyLock;

use base64::Engine as _;
use scraper::{Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, contains_any_ci, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch, TimeRange};

struct Sel {
    item: Selector,
    link: Selector,
    snippet: [Selector; 4],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    item: sel("li.b_algo"),
    link: sel("h2 a"),
    snippet: [
        sel(".b_caption p"),
        sel("p[class^='b_lineclamp']"),
        sel(".b_snippet"),
        sel("p"),
    ],
});

pub struct Bing {
    base: Url,
}

impl Bing {
    pub const DEFAULT_BASE: &'static str = "https://www.bing.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }
}

/// Bing wraps result links as `/ck/a?...&u=a1<base64url(target)>`.
fn unwrap_link(url: Url) -> Option<Url> {
    let is_bing = url.host_str().is_some_and(|h| h.ends_with("bing.com"));
    if !(is_bing && url.path().starts_with("/ck/a")) {
        return Some(url);
    }
    let u = url.query_pairs().find(|(k, _)| k == "u")?.1;
    let encoded = u.strip_prefix("a1")?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let target = Url::parse(std::str::from_utf8(&bytes).ok()?).ok()?;
    matches!(target.scheme(), "http" | "https").then_some(target)
}

/// Bing's freshness filter: `ez1..3` for day/week/month, an explicit range of
/// days since the Unix epoch for anything longer.
fn time_filter(t: TimeRange) -> String {
    match t {
        TimeRange::Day => r#"ex1:"ez1""#.into(),
        TimeRange::Week => r#"ex1:"ez2""#.into(),
        TimeRange::Month => r#"ex1:"ez3""#.into(),
        TimeRange::Year => {
            let today = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() / 86_400);
            format!(r#"ex1:"ez5_{}_{today}""#, today.saturating_sub(t.days()))
        }
    }
}

impl Engine for Bing {
    fn kind(&self) -> EngineKind {
        EngineKind::Bing
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        let mut url = self.base.join("search").expect("static path");
        let lang = q.region.language();
        let country = q.region.country().to_ascii_uppercase();
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("q", q.query)
                .append_pair("setlang", lang)
                .append_pair("mkt", &format!("{lang}-{country}"))
                .append_pair("cc", &country)
                .append_pair(
                    "adlt",
                    match q.safe {
                        SafeSearch::Off => "off",
                        SafeSearch::Moderate => "moderate",
                        SafeSearch::Strict => "strict",
                    },
                );
            if let Some(t) = q.time_range {
                p.append_pair("filters", &time_filter(t));
            }
            if q.page > 0 {
                p.append_pair("first", &(q.page * 10 + 1).to_string())
                    .append_pair("FORM", "PERE");
            }
        }
        UpstreamRequest::get(url)
    }

    fn ready_selector(&self) -> &'static str {
        "#b_results"
    }

    fn is_blocked(&self, status: u16, html: &str) -> bool {
        generic_block(status, html)
            || contains_any_ci(html, &["id=\"b_captcha", "class=\"b_captcha", "/challenge/verify"])
    }

    fn parse(&self, html: &str) -> Vec<RawResult> {
        let doc = Html::parse_document(html);
        let s = &*S;
        doc.select(&s.item)
            .filter_map(|item| {
                let a = item.select(&s.link).next()?;
                let url = unwrap_link(absolute_http(a.value().attr("href")?, &self.base)?)?;
                let title = text(a);
                if title.is_empty() {
                    return None;
                }
                Some(RawResult {
                    title,
                    url: url.into(),
                    snippet: first_text(item, &s.snippet),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ck_links() {
        let wrapped =
            Url::parse("https://www.bing.com/ck/a?!&&p=abc&u=a1aHR0cHM6Ly93d3cucnVzdC1sYW5nLm9yZy8&ntb=1").unwrap();
        assert_eq!(unwrap_link(wrapped).unwrap().as_str(), "https://www.rust-lang.org/");
        let direct = Url::parse("https://docs.rs/").unwrap();
        assert_eq!(unwrap_link(direct).unwrap().as_str(), "https://docs.rs/");
    }
}
