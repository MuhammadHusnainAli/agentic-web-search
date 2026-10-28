use std::sync::LazyLock;

use scraper::{Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch};

struct Sel {
    item: Selector,
    link: Selector,
    title: [Selector; 2],
    snippet: [Selector; 4],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    item: sel("div.snippet[data-type='web']"),
    link: sel("a[href^='http']"),
    title: [sel(".search-snippet-title"), sel(".title")],
    snippet: [
        sel(".generic-snippet .content"),
        sel(".snippet-description"),
        sel(".inline-qa-question"),
        sel(".snippet-content"),
    ],
});

pub struct Brave {
    base: Url,
}

impl Brave {
    pub const DEFAULT_BASE: &'static str = "https://search.brave.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }
}

impl Engine for Brave {
    fn kind(&self) -> EngineKind {
        EngineKind::Brave
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        let mut url = self.base.join("search").expect("static path");
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("q", q.query).append_pair("source", "web");
            if q.page > 0 {
                p.append_pair("offset", &q.page.to_string());
            }
        }
        let safe = match q.safe {
            SafeSearch::Off => "off",
            SafeSearch::Moderate => "moderate",
            SafeSearch::Strict => "strict",
        };
        // Brave reads locale & safe-search preferences from cookies.
        UpstreamRequest::get(url).header(
            "cookie",
            format!(
                "safesearch={safe}; country={}; ui_lang={}-{}; useLocation=0",
                q.region.country(),
                q.region.language(),
                q.region.country()
            ),
        )
    }

    fn ready_selector(&self) -> &'static str {
        "div.snippet[data-type='web']"
    }

    fn is_blocked(&self, status: u16, html: &str) -> bool {
        generic_block(status, html)
    }

    fn parse(&self, html: &str) -> Vec<RawResult> {
        let doc = Html::parse_document(html);
        let s = &*S;
        doc.select(&s.item)
            .filter_map(|item| {
                let a = item.select(&s.link).next()?;
                let url = absolute_http(a.value().attr("href")?, &self.base)?;
                let mut title = first_text(item, &s.title);
                if title.is_empty() {
                    title = text(a);
                }
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
