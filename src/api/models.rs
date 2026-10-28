//! Request/response contracts of the public API.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{
    config::Config,
    error::{AppError, EngineFailure},
    model::{EngineChoice, EngineKind, FetchMode, Region, SafeSearch},
    search::SearchRequest,
};

/// Max results a caller may ask for after merging pages.
pub const MAX_RESULTS_CAP: u32 = 200;

/// Body of `POST /v1/search`; also the query string of `GET /v1/search`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    #[serde(alias = "q")]
    pub query: String,
    /// `auto` (default) or one of duckduckgo|google|bing|brave|yahoo|mojeek.
    pub engine: Option<EngineChoice>,
    /// Number of SERP pages to fetch (default 1).
    pub pages: Option<u32>,
    pub mode: Option<FetchMode>,
    pub region: Option<Region>,
    pub safe_search: Option<SafeSearch>,
    pub max_results: Option<u32>,
    pub include_content: Option<bool>,
    /// How many top results get `content` extracted (default 3).
    pub content_limit: Option<u32>,
}

fn normalise_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for word in q.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

impl SearchParams {
    pub fn validate(self, cfg: &Config, browser_enabled: bool) -> Result<SearchRequest, AppError> {
        let bad = |m: String| Err(AppError::BadRequest(m));

        let query = normalise_query(&self.query);
        if query.is_empty() {
            return bad("`query` must not be empty".into());
        }
        if query.chars().count() > cfg.max_query_len {
            return bad(format!("`query` exceeds {} characters", cfg.max_query_len));
        }

        let pages = self.pages.unwrap_or(1);
        if pages == 0 || pages > cfg.max_pages {
            return bad(format!("`pages` must be between 1 and {}", cfg.max_pages));
        }

        if let Some(m) = self.max_results
            && (m == 0 || m > MAX_RESULTS_CAP)
        {
            return bad(format!("`max_results` must be between 1 and {MAX_RESULTS_CAP}"));
        }

        let include_content = self.include_content.unwrap_or(false);
        let content_limit = self.content_limit.unwrap_or(3);
        if include_content && (content_limit == 0 || content_limit as usize > cfg.content_max_results) {
            return bad(format!(
                "`content_limit` must be between 1 and {}",
                cfg.content_max_results
            ));
        }

        let mode = self.mode.unwrap_or(cfg.default_mode);
        if mode == FetchMode::Browser && !browser_enabled {
            return bad("`mode=browser` is unavailable: headless browser disabled on this server".into());
        }

        let engine = match self.engine {
            Some(e) => e,
            None => cfg.default_engine.map_or(EngineChoice::Auto, EngineChoice::Engine),
        };

        Ok(SearchRequest {
            query,
            engine,
            pages,
            mode,
            region: self.region.unwrap_or(cfg.default_region),
            safe: self.safe_search.unwrap_or_default(),
            max_results: self.max_results,
            include_content,
            // Normalise so it doesn't fragment the cache when content is off.
            content_limit: if include_content { content_limit } else { 0 },
        })
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResponse<'a> {
    pub query: &'a str,
    pub engine: EngineKind,
    pub engines_tried: &'a [EngineKind],
    pub pages_requested: u32,
    pub pages_fetched: u32,
    pub total_results: usize,
    pub cached: bool,
    pub elapsed_ms: u64,
    pub results: &'a RawValue,
    pub errors: &'a [EngineFailure],
}

#[derive(Debug, Serialize)]
pub struct EngineInfo {
    pub name: EngineKind,
    pub requires_browser: bool,
    pub available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(query: &str) -> SearchParams {
        SearchParams {
            query: query.into(),
            engine: None,
            pages: None,
            mode: None,
            region: None,
            safe_search: None,
            max_results: None,
            include_content: None,
            content_limit: None,
        }
    }

    #[test]
    fn normalises_and_defaults() {
        let cfg = Config::default();
        let r = params("  rust \n  async ").validate(&cfg, true).unwrap();
        assert_eq!(r.query, "rust async");
        assert_eq!(r.pages, 1);
        assert_eq!(r.engine, EngineChoice::Auto);
        assert_eq!(r.content_limit, 0);
    }

    #[test]
    fn rejects_invalid() {
        let cfg = Config::default();
        assert!(params("   ").validate(&cfg, true).is_err());
        let mut p = params("x");
        p.pages = Some(cfg.max_pages + 1);
        assert!(p.validate(&cfg, true).is_err());
        let mut p = params("x");
        p.mode = Some(FetchMode::Browser);
        assert!(p.validate(&cfg, false).is_err());
        let mut p = params("x");
        p.max_results = Some(0);
        assert!(p.validate(&cfg, true).is_err());
        assert!(params(&"a".repeat(cfg.max_query_len + 1)).validate(&cfg, true).is_err());
    }
}
