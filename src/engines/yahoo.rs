use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use scraper::{Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch};

/// Yahoo renders 7 organic results per page.
const PER_PAGE: u32 = 7;

struct Sel {
    item: Selector,
    link: Selector,
    title: Selector,
    snippet: [Selector; 2],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    item: sel("div.algo-sr"),
    link: sel(".compTitle a[href], h3 a[href]"),
    title: sel("h3"),
    snippet: [sel(".compText"), sel("p")],
});

pub struct Yahoo {
    base: Url,
}

impl Yahoo {
    pub const DEFAULT_BASE: &'static str = "https://search.yahoo.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }
}

/// Yahoo wraps links as `https://r.search.yahoo.com/.../RU=<pct-encoded>/RK=.../RS=...`.
fn unwrap_link(url: Url) -> Option<Url> {
    if !url.host_str().is_some_and(|h| h.starts_with("r.search.yahoo.")) {
        return Some(url);
    }
    let path = url.path();
    let start = path.find("/RU=")? + 4;
    let end = path[start..].find("/R").map_or(path.len(), |e| start + e);
    let target = percent_decode_str(&path[start..end]).decode_utf8().ok()?;
    let target = Url::parse(&target).ok()?;
    matches!(target.scheme(), "http" | "https").then_some(target)
}

impl Engine for Yahoo {
    fn kind(&self) -> EngineKind {
        EngineKind::Yahoo
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        let mut url = self.base.join("search").expect("static path");
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("p", q.query)
                .append_pair("ei", "UTF-8")
                .append_pair("vl", &format!("lang_{}", q.region.language()))
                .append_pair(
                    "vm",
                    match q.safe {
                        SafeSearch::Off => "p",
                        SafeSearch::Moderate => "i",
                        SafeSearch::Strict => "r",
                    },
                );
            if q.page > 0 {
                p.append_pair("b", &(q.page * PER_PAGE + 1).to_string());
            }
        }
        UpstreamRequest::get(url)
    }

    fn ready_selector(&self) -> &'static str {
        // `#web` exists before results stream in; wait for an actual result.
        "div.algo-sr"
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
                let url = unwrap_link(absolute_http(a.value().attr("href")?, &self.base)?)?;
                let title = item
                    .select(&s.title)
                    .next()
                    .map(text)
                    .filter(|t| !t.is_empty())
                    .or_else(|| a.value().attr("aria-label").map(str::to_owned))?;
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
    fn unwraps_ru_links() {
        let u = Url::parse(
            "https://r.search.yahoo.com/_ylt=Awr/RV=2/RE=1/RO=10/RU=https%3a%2f%2fwww.rust-lang.org%2f/RK=2/RS=abc-",
        )
        .unwrap();
        assert_eq!(unwrap_link(u).unwrap().as_str(), "https://www.rust-lang.org/");
    }
}
