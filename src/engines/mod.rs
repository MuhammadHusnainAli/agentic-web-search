//! Search engine adapters.
//!
//! An engine is pure and synchronous: it knows how to build the upstream request
//! for one SERP page, how to recognise a block/challenge page, and how to parse
//! results out of HTML. All I/O lives in [`crate::fetch`].

mod bing;
mod brave;
mod duckduckgo;
mod google;
mod mojeek;
pub mod util;
mod yahoo;

pub use bing::Bing;
pub use brave::Brave;
pub use duckduckgo::DuckDuckGo;
pub use google::Google;
pub use mojeek::Mojeek;
pub use yahoo::Yahoo;

use url::Url;

use crate::{
    config::Config,
    model::{EngineKind, RawResult, Region, SafeSearch, TimeRange},
};

/// Parameters for fetching a single SERP page.
#[derive(Debug, Clone, Copy)]
pub struct PageQuery<'a> {
    pub query: &'a str,
    /// 0-based page index.
    pub page: u32,
    pub region: Region,
    pub safe: SafeSearch,
    pub time_range: Option<TimeRange>,
}

/// A fully described upstream HTTP request. `POST` with a form body when `form` is set.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub url: Url,
    pub form: Option<Vec<(&'static str, String)>>,
    pub headers: Vec<(&'static str, String)>,
}

impl UpstreamRequest {
    pub fn get(url: Url) -> Self {
        Self {
            url,
            form: None,
            headers: Vec::new(),
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

pub trait Engine: Send + Sync + 'static {
    fn kind(&self) -> EngineKind;

    /// Request used by the plain-HTTP fetcher.
    fn http_request(&self, q: &PageQuery<'_>) -> UpstreamRequest;

    /// GET-able URL used by the headless browser, or `None` when this page
    /// can't be reached with a plain navigation (e.g. form-POST pagination).
    fn browser_url(&self, q: &PageQuery<'_>) -> Option<Url> {
        let req = self.http_request(q);
        req.form.is_none().then_some(req.url)
    }

    /// CSS selector that indicates results have rendered (browser mode).
    fn ready_selector(&self) -> &'static str;

    /// `true` when the plain-HTTP SERP is unusable without JavaScript.
    fn requires_js(&self) -> bool {
        false
    }

    /// Whether the engine can filter by `range` natively.
    fn supports_time_range(&self, _range: TimeRange) -> bool {
        true
    }

    /// Detect CAPTCHA / bot-challenge / JS-wall responses.
    fn is_blocked(&self, status: u16, html: &str) -> bool;

    /// Extract organic results (ads and widgets excluded).
    fn parse(&self, html: &str) -> Vec<RawResult>;

    /// Results plus page-level extras. Engines that surface a spelling
    /// correction in their HTML override this to parse the page only once.
    fn parse_serp(&self, html: &str) -> Serp {
        Serp {
            results: self.parse(html),
            correction: None,
        }
    }
}

/// What one SERP page yields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Serp {
    pub results: Vec<RawResult>,
    /// The engine's "did you mean" / "including results for" query.
    pub correction: Option<String>,
}

/// Fixed-size registry indexed by [`EngineKind::index`].
pub struct Engines {
    list: [Box<dyn Engine>; 6],
}

fn base(cfg: &Config, kind: EngineKind, default: &str) -> Url {
    let raw = cfg.base_url(kind).unwrap_or(default);
    // Config validates overrides; defaults are constants.
    Url::parse(raw).unwrap_or_else(|e| panic!("invalid base url for {kind}: {e}"))
}

impl Engines {
    pub fn new(cfg: &Config) -> Self {
        // Order must match EngineKind discriminants.
        let list: [Box<dyn Engine>; 6] = [
            Box::new(DuckDuckGo::new(base(
                cfg,
                EngineKind::DuckDuckGo,
                DuckDuckGo::DEFAULT_BASE,
            ))),
            Box::new(Google::new(base(cfg, EngineKind::Google, Google::DEFAULT_BASE))),
            Box::new(Bing::new(base(cfg, EngineKind::Bing, Bing::DEFAULT_BASE))),
            Box::new(Brave::new(base(cfg, EngineKind::Brave, Brave::DEFAULT_BASE))),
            Box::new(Yahoo::new(base(cfg, EngineKind::Yahoo, Yahoo::DEFAULT_BASE))),
            Box::new(Mojeek::new(base(cfg, EngineKind::Mojeek, Mojeek::DEFAULT_BASE))),
        ];
        debug_assert!(list.iter().enumerate().all(|(i, e)| e.kind().index() == i));
        Self { list }
    }

    pub fn get(&self, kind: EngineKind) -> &dyn Engine {
        self.list[kind.index()].as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_kinds() {
        let engines = Engines::new(&Config::default());
        for kind in EngineKind::ALL {
            assert_eq!(engines.get(kind).kind(), kind);
        }
    }

    #[test]
    fn base_url_override_is_used() {
        let mut cfg = Config::default();
        cfg.engine_base_urls
            .insert(EngineKind::Bing, "http://127.0.0.1:9999/".into());
        let engines = Engines::new(&cfg);
        let q = PageQuery {
            query: "rust",
            page: 1,
            region: Region::DEFAULT,
            safe: SafeSearch::Moderate,
            time_range: None,
        };
        let req = engines.get(EngineKind::Bing).http_request(&q);
        assert!(req.url.as_str().starts_with("http://127.0.0.1:9999/search?"));
        assert!(req.url.as_str().contains("first=11"));
    }
}
