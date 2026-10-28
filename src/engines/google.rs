use std::{collections::HashSet, sync::LazyLock};

use scraper::{ElementRef, Html, Selector};
use url::Url;

use super::{
    Engine, PageQuery, UpstreamRequest,
    util::{absolute_http, contains_any_ci, first_text, generic_block, sel, text},
};
use crate::model::{EngineKind, RawResult, SafeSearch};

struct Sel {
    heading: Selector,
    snippet: [Selector; 5],
}

static S: LazyLock<Sel> = LazyLock::new(|| Sel {
    heading: sel("#search h3, #rso h3, #main h3, #botstuff h3"),
    snippet: [
        sel("[data-sncf]"),
        sel(".VwiC3b"),
        sel("[style*='-webkit-line-clamp']"),
        sel(".BNeawe.s3v9rd"),
        sel(".lEBKkf"),
    ],
});

/// Class names / attributes Google uses for an organic result container.
const CONTAINER_CLASSES: &[&str] = &["g", "MjjYud", "tF2Cxc", "ezO2md", "Gx5Zad"];

pub struct Google {
    base: Url,
}

impl Google {
    pub const DEFAULT_BASE: &'static str = "https://www.google.com/";

    pub fn new(base: Url) -> Self {
        Self { base }
    }
}

/// Unwrap `/url?q=<target>` (no-JS SERP) and drop internal Google links.
fn unwrap_link(url: Url) -> Option<Url> {
    let host = url.host_str().unwrap_or_default();
    let is_google = host == "google.com" || host.ends_with(".google.com") || host.starts_with("google.");
    if !is_google {
        return Some(url);
    }
    if url.path() == "/url" {
        let target = url.query_pairs().find(|(k, _)| k == "q" || k == "url")?.1;
        let target = Url::parse(&target).ok()?;
        return matches!(target.scheme(), "http" | "https").then_some(target);
    }
    None
}

fn container(a: ElementRef<'_>) -> Option<ElementRef<'_>> {
    a.ancestors().filter_map(ElementRef::wrap).take(10).find(|e| {
        let v = e.value();
        v.classes().any(|c| CONTAINER_CLASSES.contains(&c)) || v.attr("data-hveid").is_some()
    })
}

impl Engine for Google {
    fn kind(&self) -> EngineKind {
        EngineKind::Google
    }

    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest {
        let mut url = self.base.join("search").expect("static path");
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("q", q.query)
                .append_pair("hl", q.region.language())
                .append_pair("gl", q.region.country())
                .append_pair("ie", "UTF-8")
                .append_pair("num", "10");
            match q.safe {
                SafeSearch::Off => {
                    p.append_pair("safe", "off");
                }
                SafeSearch::Strict => {
                    p.append_pair("safe", "active");
                }
                SafeSearch::Moderate => {}
            }
            if let Some(t) = q.time_range {
                p.append_pair("tbs", &format!("qdr:{}", t.letter()));
            }
            if q.page > 0 {
                p.append_pair("start", &(q.page * 10).to_string());
            }
        }
        // Pre-accept the EU consent interstitial.
        UpstreamRequest::get(url).header("cookie", "CONSENT=YES+; SOCS=CAI")
    }

    fn ready_selector(&self) -> &'static str {
        "#search, #rso, #botstuff, #captcha-form"
    }

    fn requires_js(&self) -> bool {
        true
    }

    fn is_blocked(&self, status: u16, html: &str) -> bool {
        generic_block(status, html)
            || contains_any_ci(
                html,
                &[
                    "/sorry/index",
                    "unusual traffic from your computer",
                    "id=\"captcha-form",
                    "/httpservice/retry/enablejs",
                ],
            )
    }

    fn parse(&self, html: &str) -> Vec<RawResult> {
        let doc = Html::parse_document(html);
        let s = &*S;
        let mut seen = HashSet::new();
        doc.select(&s.heading)
            .filter_map(|h3| {
                let a = h3
                    .ancestors()
                    .filter_map(ElementRef::wrap)
                    .take(4)
                    .find(|e| e.value().name() == "a")?;
                let url = unwrap_link(absolute_http(a.value().attr("href")?, &self.base)?)?;
                let title = text(h3);
                if title.is_empty() || !seen.insert(url.as_str().to_owned()) {
                    return None;
                }
                let snippet = container(a).map(|c| first_text(c, &s.snippet)).unwrap_or_default();
                Some(RawResult {
                    title,
                    url: url.into(),
                    snippet,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_url_redirects_and_drops_internal() {
        let u = Url::parse("https://www.google.com/url?q=https://tokio.rs/&sa=U").unwrap();
        assert_eq!(unwrap_link(u).unwrap().as_str(), "https://tokio.rs/");
        let internal = Url::parse("https://www.google.com/search?q=related").unwrap();
        assert!(unwrap_link(internal).is_none());
        let ext = Url::parse("https://docs.rs/").unwrap();
        assert!(unwrap_link(ext).is_some());
    }
}
