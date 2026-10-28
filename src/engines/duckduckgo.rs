use std::sync::LazyLock;

use scraper::{Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, contains_any_ci, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch};

struct Sel {
    /// No-JS SERP (`html.duckduckgo.com`).
    item: Selector,
    link: Selector,
    snippet: [Selector; 1],
    /// JS SERP (`duckduckgo.com`), seen when rendered in the browser.
    js_item: Selector,
    js_link: Selector,
    js_snippet: [Selector; 1],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    item: sel("div.result:not(.result--ad)"),
    link: sel("a.result__a"),
    snippet: [sel(".result__snippet")],
    js_item: sel("article[data-testid='result']"),
    js_link: sel("a[data-testid='result-title-a']"),
    js_snippet: [sel("[data-result='snippet']")],
});

pub struct DuckDuckGo {
    base: Url,
}

impl DuckDuckGo {
    pub const DEFAULT_BASE: &'static str = "https://html.duckduckgo.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }

    fn params(q: &PageQuery<'_>) -> Vec<(&'static str, String)> {
        let offset = q.page * 10;
        let mut p = vec![
            ("q", q.query.to_owned()),
            ("kl", q.region.to_string()),
            (
                "kp",
                match q.safe {
                    SafeSearch::Off => "-2",
                    SafeSearch::Moderate => "-1",
                    SafeSearch::Strict => "1",
                }
                .to_owned(),
            ),
        ];
        if offset > 0 {
            // Mirrors the hidden fields of DDG's own "Next" form.
            p.push(("s", offset.to_string()));
            p.push(("dc", (offset + 1).to_string()));
            p.push(("v", "l".to_owned()));
            p.push(("o", "json".to_owned()));
            p.push(("api", "d.js".to_owned()));
            p.push(("nextParams", String::new()));
        }
        p
    }
}

/// DDG wraps result links as `//duckduckgo.com/l/?uddg=<percent-encoded target>`.
fn unwrap_link(url: Url) -> Option<Url> {
    let is_redirect = url.host_str().is_some_and(|h| h.ends_with("duckduckgo.com")) && url.path().starts_with("/l/");
    if !is_redirect {
        return Some(url);
    }
    let target = url.query_pairs().find(|(k, _)| k == "uddg")?.1;
    let target = Url::parse(&target).ok()?;
    matches!(target.scheme(), "http" | "https").then_some(target)
}

impl Engine for DuckDuckGo {
    fn kind(&self) -> EngineKind {
        EngineKind::DuckDuckGo
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        UpstreamRequest {
            url: self.base.join("html/").expect("static path"),
            form: Some(Self::params(q)),
            headers: Vec::new(),
        }
    }

    /// The no-JS SERP ignores offsets on GET (paging is a form POST), so only
    /// page 1 can be rendered in the browser.
    fn browser_url(&self, q: &PageQuery<'_>) -> Option<Url> {
        if q.page > 0 {
            return None;
        }
        let mut url = self.base.join("html/").expect("static path");
        url.query_pairs_mut()
            .extend_pairs(Self::params(q).iter().map(|(k, v)| (*k, v.as_str())));
        Some(url)
    }

    fn ready_selector(&self) -> &'static str {
        ".result, .no-results, article[data-testid='result']"
    }

    fn is_blocked(&self, status: u16, html: &str) -> bool {
        generic_block(status, html)
            || contains_any_ci(html, &["anomaly-modal", "bots use duckduckgo too", "challenge-form"])
    }

    fn parse(&self, html: &str) -> Vec<RawResult> {
        let doc = Html::parse_document(html);
        let s = &*S;
        let mut out: Vec<RawResult> = doc
            .select(&s.item)
            .filter_map(|item| self.extract(item, &s.link, &s.snippet))
            .collect();
        if out.is_empty() {
            out = doc
                .select(&s.js_item)
                .filter_map(|item| self.extract(item, &s.js_link, &s.js_snippet))
                .collect();
        }
        out
    }
}

impl DuckDuckGo {
    fn extract(&self, item: scraper::ElementRef<'_>, link: &Selector, snippet: &[Selector]) -> Option<RawResult> {
        let a = item.select(link).next()?;
        let url = unwrap_link(absolute_http(a.value().attr("href")?, &self.base)?)?;
        // Ad links go through duckduckgo.com/y.js
        if url.host_str().is_some_and(|h| h.ends_with("duckduckgo.com")) {
            return None;
        }
        let title = text(a);
        (!title.is_empty()).then(|| RawResult {
            title,
            url: url.into(),
            snippet: first_text(item, snippet),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_uddg_links() {
        let base = Url::parse(DuckDuckGo::DEFAULT_BASE).unwrap();
        let wrapped = absolute_http(
            "//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&rut=abc",
            &base,
        )
        .unwrap();
        assert_eq!(unwrap_link(wrapped).unwrap().as_str(), "https://rust-lang.org/");
    }

    #[test]
    fn page_offsets() {
        let q = PageQuery {
            query: "x",
            page: 2,
            region: Default::default(),
            safe: SafeSearch::Strict,
        };
        let p = DuckDuckGo::params(&q);
        assert!(p.contains(&("s", "20".into())));
        assert!(p.contains(&("dc", "21".into())));
        assert!(p.contains(&("kp", "1".into())));
        assert!(p.contains(&("kl", "us-en".into())));
    }
}
