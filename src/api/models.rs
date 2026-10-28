//! Request/response contracts of the public API.

use std::{collections::HashSet, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use url::Url;

use crate::{
    config::Config,
    error::{AppError, EngineFailure},
    model::{ContentFormat, ContentMode, EngineChoice, EngineKind, FetchMode, Region, SafeSearch, Strategy, TimeRange},
    search::SearchRequest,
};

/// Max results a caller may ask for after merging pages.
pub const MAX_RESULTS_CAP: u32 = 200;
/// Max entries in `include_domains` / `exclude_domains`.
pub const MAX_DOMAINS: usize = 10;
/// Bounds of the per-request content deadline.
pub const CONTENT_TIMEOUT_MS: std::ops::RangeInclusive<u32> = 100..=30_000;

/// Body of `POST /v1/search`; also the query string of `GET /v1/search`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    #[serde(alias = "q")]
    pub query: String,
    /// `auto` (default) or one of duckduckgo|google|bing|brave|yahoo|mojeek.
    pub engine: Option<EngineChoice>,
    /// Engines `auto` may use, in order. Array, or comma-separated in a query string.
    #[serde(default, deserialize_with = "list")]
    pub engines: Option<Vec<EngineKind>>,
    /// How `auto` combines engines: fallback (default) | race | merge.
    pub strategy: Option<Strategy>,
    /// Number of SERP pages to fetch (default 1).
    pub pages: Option<u32>,
    pub mode: Option<FetchMode>,
    pub region: Option<Region>,
    pub safe_search: Option<SafeSearch>,
    /// day | week | month | year
    pub time_range: Option<TimeRange>,
    /// Only return results from these domains (subdomains included).
    #[serde(default, alias = "site", deserialize_with = "list")]
    pub include_domains: Option<Vec<String>>,
    /// Never return results from these domains (subdomains included).
    #[serde(default, deserialize_with = "list")]
    pub exclude_domains: Option<Vec<String>>,
    /// Restrict to a file type, e.g. `pdf`.
    pub file_type: Option<String>,
    /// Keep only the best-ranked result per domain.
    pub unique_domains: Option<bool>,
    pub max_results: Option<u32>,
    pub include_content: Option<bool>,
    /// How many top results get `content` extracted (default 3).
    pub content_limit: Option<u32>,
    /// markdown (default) | text
    pub content_format: Option<ContentFormat>,
    /// full (default) | relevant: only the passages matching the query.
    pub content_mode: Option<ContentMode>,
    /// Max characters of `content` per result (default and cap: `CONTENT_MAX_CHARS`).
    pub content_max_chars: Option<u32>,
    /// Deadline for content extraction; slower pages come back without `content`.
    pub content_timeout_ms: Option<u32>,
    /// Bypass the cache for this request (the fresh result is still cached).
    pub fresh: Option<bool>,
}

/// Accept either a sequence or a comma-separated string (query strings can't
/// carry arrays).
fn list<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<String>),
        One(String),
    }
    let items = match Option::<OneOrMany>::deserialize(d)? {
        None => return Ok(None),
        Some(OneOrMany::Many(v)) => v,
        Some(OneOrMany::One(s)) => s.split(',').map(str::to_owned).collect(),
    };
    items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(serde::de::Error::custom))
        .collect::<Result<Vec<T>, _>>()
        .map(Some)
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

/// Reduce `https://www.Docs.rs/x` / `docs.rs` style input to a bare host.
fn normalise_domain(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_ascii_lowercase();
    let host = if s.contains("://") {
        Url::parse(&s).ok().and_then(|u| u.host_str().map(str::to_owned))
    } else {
        s.split(['/', '?', '#']).next().map(str::to_owned)
    }
    .unwrap_or_default();
    let host = host.trim_start_matches("www.").trim_end_matches('.');
    let valid = host.len() <= 253
        && host.contains('.')
        && !host.starts_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if valid {
        Ok(host.to_owned())
    } else {
        Err(format!("invalid domain `{raw}`"))
    }
}

fn domains(field: &str, raw: Option<Vec<String>>) -> Result<Vec<String>, AppError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > MAX_DOMAINS {
        return Err(AppError::BadRequest(format!(
            "`{field}` accepts at most {MAX_DOMAINS} domains"
        )));
    }
    let mut out = raw
        .iter()
        .map(|d| normalise_domain(d))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::BadRequest(format!("`{field}`: {e}")))?;
    // Order-insensitive so equivalent requests share a cache entry.
    out.sort_unstable();
    out.dedup();
    Ok(out)
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
        let max_chars = cfg.content_max_chars as u32;
        let content_max_chars = self.content_max_chars.unwrap_or(max_chars);
        if !(100..=max_chars).contains(&content_max_chars) {
            return bad(format!("`content_max_chars` must be between 100 and {max_chars}"));
        }
        let content_timeout_ms = self
            .content_timeout_ms
            .unwrap_or(cfg.content_timeout.as_millis() as u32);
        if !CONTENT_TIMEOUT_MS.contains(&content_timeout_ms) {
            return bad(format!(
                "`content_timeout_ms` must be between {} and {}",
                CONTENT_TIMEOUT_MS.start(),
                CONTENT_TIMEOUT_MS.end()
            ));
        }

        let mode = self.mode.unwrap_or(cfg.default_mode);
        if mode == FetchMode::Browser && !browser_enabled {
            return bad("`mode=browser` is unavailable: headless browser disabled on this server".into());
        }

        let mut engines = self.engines.unwrap_or_default();
        let mut seen = HashSet::new();
        engines.retain(|e| seen.insert(*e));
        let engine = match (self.engine, engines.is_empty()) {
            (Some(EngineChoice::Engine(e)), false) => {
                return bad(format!(
                    "`engine={e}` conflicts with `engines`; use `engine=auto` with an engine list"
                ));
            }
            (Some(e), _) => e,
            (None, false) => EngineChoice::Auto,
            (None, true) => cfg.default_engine.map_or(EngineChoice::Auto, EngineChoice::Engine),
        };
        if self.strategy.is_some() && engine != EngineChoice::Auto {
            return bad("`strategy` only applies to `engine=auto`".into());
        }

        let include_domains = domains("include_domains", self.include_domains)?;
        let exclude_domains = domains("exclude_domains", self.exclude_domains)?;
        if let Some(d) = include_domains.iter().find(|d| exclude_domains.contains(d)) {
            return bad(format!("`{d}` is both included and excluded"));
        }
        let file_type = match self.file_type.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(ft) => {
                let ft = ft.trim_start_matches('.').to_ascii_lowercase();
                if ft.is_empty() || ft.len() > 8 || !ft.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return bad(format!("invalid `file_type` `{ft}`"));
                }
                Some(ft)
            }
        };

        Ok(SearchRequest {
            query,
            engine,
            engines,
            strategy: self.strategy.unwrap_or_default(),
            pages,
            mode,
            region: self.region.unwrap_or(cfg.default_region),
            safe: self.safe_search.unwrap_or_default(),
            time_range: self.time_range,
            include_domains,
            exclude_domains,
            file_type,
            unique_domains: self.unique_domains.unwrap_or(false),
            max_results: self.max_results,
            include_content,
            // Normalise so content knobs don't fragment the cache when content is off.
            content_limit: if include_content { content_limit } else { 0 },
            content_format: if include_content {
                self.content_format.unwrap_or_default()
            } else {
                ContentFormat::default()
            },
            content_mode: if include_content {
                self.content_mode.unwrap_or_default()
            } else {
                ContentMode::default()
            },
            content_max_chars: if include_content { content_max_chars } else { 0 },
            content_timeout_ms: if include_content { content_timeout_ms } else { 0 },
        })
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResponse<'a> {
    pub query: &'a str,
    pub engine: EngineKind,
    pub engines_tried: &'a [EngineKind],
    /// Engines whose results are in `results` (several with `strategy=merge`).
    pub engines_used: &'a [EngineKind],
    pub pages_requested: u32,
    pub pages_fetched: u32,
    pub total_results: usize,
    pub cached: bool,
    pub elapsed_ms: u64,
    pub results: &'a RawValue,
    pub errors: &'a [EngineFailure],
    /// Non-fatal notes, e.g. an engine that ignored `time_range`.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    pub warnings: &'a [String],
}

#[derive(Debug, Serialize)]
pub struct EngineInfo {
    pub name: EngineKind,
    pub requires_browser: bool,
    pub available: bool,
    /// Time ranges the engine filters natively.
    pub time_ranges: Vec<TimeRange>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(query: &str) -> SearchParams {
        SearchParams {
            query: query.into(),
            engine: None,
            engines: None,
            strategy: None,
            pages: None,
            mode: None,
            region: None,
            safe_search: None,
            time_range: None,
            include_domains: None,
            exclude_domains: None,
            file_type: None,
            unique_domains: None,
            max_results: None,
            include_content: None,
            content_limit: None,
            content_format: None,
            content_mode: None,
            content_max_chars: None,
            content_timeout_ms: None,
            fresh: None,
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
        let mut p = params("x");
        p.engine = Some(EngineChoice::Engine(EngineKind::Bing));
        p.engines = Some(vec![EngineKind::Brave]);
        assert!(p.validate(&cfg, true).is_err());
        let mut p = params("x");
        p.engine = Some(EngineChoice::Engine(EngineKind::Bing));
        p.strategy = Some(Strategy::Merge);
        assert!(p.validate(&cfg, true).is_err());
        let mut p = params("x");
        p.include_domains = Some(vec!["a.com".into()]);
        p.exclude_domains = Some(vec!["www.A.com".into()]);
        assert!(p.validate(&cfg, true).is_err());
        let mut p = params("x");
        p.file_type = Some("p d f".into());
        assert!(p.validate(&cfg, true).is_err());
    }

    #[test]
    fn normalises_filters() {
        let cfg = Config::default();
        let mut p = params("x");
        p.include_domains = Some(vec![
            "https://www.Docs.rs/tokio".into(),
            "docs.rs".into(),
            "a.io".into(),
        ]);
        p.file_type = Some(".PDF".into());
        p.engines = Some(vec![EngineKind::Bing, EngineKind::Brave, EngineKind::Bing]);
        let r = p.validate(&cfg, true).unwrap();
        assert_eq!(r.include_domains, ["a.io", "docs.rs"]);
        assert_eq!(r.file_type.as_deref(), Some("pdf"));
        assert_eq!(r.engines, [EngineKind::Bing, EngineKind::Brave]);
        assert_eq!(r.engine, EngineChoice::Auto);
        assert!(normalise_domain("localhost").is_err());
        assert!(normalise_domain("a b.com").is_err());
    }

    #[test]
    fn lists_accept_csv_and_arrays() {
        let p: SearchParams = serde_json::from_value(serde_json::json!({
            "query": "x", "engines": "google, bing", "site": ["docs.rs"]
        }))
        .unwrap();
        assert_eq!(p.engines.unwrap(), [EngineKind::Google, EngineKind::Bing]);
        assert_eq!(p.include_domains.unwrap(), ["docs.rs"]);
        let bad = serde_json::from_value::<SearchParams>(serde_json::json!({"query": "x", "engines": "altavista"}));
        assert!(bad.is_err());
    }
}
