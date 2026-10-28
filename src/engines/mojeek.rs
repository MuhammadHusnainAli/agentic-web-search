use std::sync::LazyLock;

use scraper::{Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, contains_any_ci, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch};

struct Sel {
    item: Selector,
    link: Selector,
    snippet: [Selector; 1],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    item: sel("ul.results-standard > li"),
    link: sel("a.title, h2 a"),
    snippet: [sel("p.s")],
});

pub struct Mojeek {
    base: Url,
}

impl Mojeek {
    pub const DEFAULT_BASE: &'static str = "https://www.mojeek.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }
}

impl Engine for Mojeek {
    fn kind(&self) -> EngineKind {
        EngineKind::Mojeek
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        let mut url = self.base.join("search").expect("static path");
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("q", q.query)
                .append_pair("lb", q.region.language())
                .append_pair("arc", q.region.country());
            if matches!(q.safe, SafeSearch::Strict | SafeSearch::Moderate) {
                p.append_pair("safe", "1");
            }
            if q.page > 0 {
                p.append_pair("s", &(q.page * 10 + 1).to_string());
            }
        }
        UpstreamRequest::get(url)
    }

    fn ready_selector(&self) -> &'static str {
        "ul.results-standard, .no-results"
    }

    fn is_blocked(&self, status: u16, html: &str) -> bool {
        generic_block(status, html) || contains_any_ci(html, &["javascript is required to complete this challenge"])
    }

    fn parse(&self, html: &str) -> Vec<RawResult> {
        let doc = Html::parse_document(html);
        let s = &*S;
        doc.select(&s.item)
            .filter_map(|item| {
                let a = item.select(&s.link).next()?;
                let url = absolute_http(a.value().attr("href")?, &self.base)?;
                let title = text(a);
                (!title.is_empty()).then(|| RawResult {
                    title,
                    url: url.into(),
                    snippet: first_text(item, &s.snippet),
                })
            })
            .collect()
    }
}
