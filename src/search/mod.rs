//! Search orchestration: caching, page fan-out, engine fallback, merging and
//! optional content extraction.

pub mod content;

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{StreamExt, future::join_all, stream};
use moka::{Expiry, future::Cache};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::sync::Semaphore;
use url::Url;

use crate::{
    config::Config,
    engines::{Engine, Engines, PageQuery, util::domain_of},
    error::{AppError, EngineFailure, UpstreamError},
    fetch::{BrowserPool, HttpFetcher},
    metrics::Metrics,
    model::{EngineChoice, EngineKind, FetchMode, RawResult, Region, SafeSearch, SearchResult},
};

/// A validated search request. Also the cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchRequest {
    pub query: String,
    pub engine: EngineChoice,
    pub pages: u32,
    pub mode: FetchMode,
    pub region: Region,
    pub safe: SafeSearch,
    pub max_results: Option<u32>,
    pub include_content: bool,
    pub content_limit: u32,
}

/// The cacheable part of a search response. Results are stored pre-serialised
/// so cache hits never re-encode them.
#[derive(Debug, Serialize)]
pub struct SearchOutput {
    pub engine: EngineKind,
    pub engines_tried: Vec<EngineKind>,
    pub pages_fetched: u32,
    pub total_results: usize,
    pub results: Box<RawValue>,
    pub errors: Vec<EngineFailure>,
    #[serde(skip)]
    partial: bool,
}

struct OutputExpiry {
    ttl: Duration,
}

impl Expiry<SearchRequest, Arc<SearchOutput>> for OutputExpiry {
    fn expire_after_create(&self, _: &SearchRequest, v: &Arc<SearchOutput>, _: Instant) -> Option<Duration> {
        // Partial results (some pages failed) are kept briefly so a retry can heal them.
        Some(if v.partial || v.total_results == 0 {
            (self.ttl / 10).max(Duration::from_secs(5)).min(self.ttl)
        } else {
            self.ttl
        })
    }
}

struct EngineRun {
    engine: EngineKind,
    pages_fetched: u32,
    results: Vec<SearchResult>,
    failures: Vec<EngineFailure>,
}

pub struct SearchService {
    cfg: Arc<Config>,
    engines: Engines,
    http: HttpFetcher,
    browser: Arc<BrowserPool>,
    cache: Cache<SearchRequest, Arc<SearchOutput>>,
    upstream: Semaphore,
    content_permits: Semaphore,
    metrics: Arc<Metrics>,
    /// Per-engine "HTTP blocked until" timestamp, ms since `epoch`.
    http_blocked_until: [AtomicU64; EngineKind::ALL.len()],
    epoch: Instant,
}

impl SearchService {
    pub fn new(cfg: Arc<Config>, metrics: Arc<Metrics>) -> Result<Self, AppError> {
        let http = HttpFetcher::new(&cfg).map_err(|e| AppError::Internal(format!("http client: {e}")))?;
        let browser = Arc::new(BrowserPool::new(
            cfg.browser.clone(),
            cfg.proxy_url.clone(),
            cfg.user_agents[0].clone(),
        ));
        let cache = Cache::builder()
            .max_capacity(cfg.cache_max_bytes)
            .weigher(|k: &SearchRequest, v: &Arc<SearchOutput>| {
                let bytes = k.query.len() + v.results.get().len() + 256 + v.errors.len() * 128;
                u32::try_from(bytes).unwrap_or(u32::MAX)
            })
            .expire_after(OutputExpiry { ttl: cfg.cache_ttl })
            .build();
        Ok(Self {
            engines: Engines::new(&cfg),
            http,
            browser,
            cache,
            upstream: Semaphore::new(cfg.max_upstream_concurrency),
            content_permits: Semaphore::new(cfg.content_max_concurrency),
            metrics,
            http_blocked_until: Default::default(),
            epoch: Instant::now(),
            cfg,
        })
    }

    pub fn engines(&self) -> &Engines {
        &self.engines
    }

    pub fn browser(&self) -> &BrowserPool {
        &self.browser
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        (self.cache.entry_count(), self.cache.weighted_size())
    }

    /// Run a search, coalescing identical concurrent requests into one upstream
    /// fetch. Returns the output and whether it was served from cache.
    pub async fn search(&self, req: &SearchRequest) -> Result<(Arc<SearchOutput>, bool), AppError> {
        let computed = AtomicBool::new(false);
        let result = self
            .cache
            .try_get_with_by_ref(req, async {
                computed.store(true, Ordering::Relaxed);
                self.execute(req).await.map(Arc::new)
            })
            .await;
        let cached = !computed.load(Ordering::Relaxed);
        Metrics::inc(if cached {
            &self.metrics.cache_hits
        } else {
            &self.metrics.cache_misses
        });
        match result {
            Ok(out) => Ok((out, cached)),
            Err(e) => {
                Metrics::inc(&self.metrics.search_failures);
                Err((*e).clone())
            }
        }
    }

    async fn execute(&self, req: &SearchRequest) -> Result<SearchOutput, AppError> {
        let (run, tried, mut failures) = match req.engine {
            EngineChoice::Engine(kind) => {
                let run = self.run_engine(kind, req).await;
                if run.pages_fetched == 0 {
                    return Err(all_failed(run.failures));
                }
                (run, vec![kind], Vec::new())
            }
            EngineChoice::Auto => self.run_auto(req).await?,
        };

        let mut results = run.results;
        if let Some(max) = req.max_results {
            results.truncate(max as usize);
        }
        if req.include_content {
            self.attach_content(&mut results, req.content_limit as usize).await;
        }
        failures.extend(run.failures);

        let raw = serde_json::value::to_raw_value(&results).map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(SearchOutput {
            engine: run.engine,
            engines_tried: tried,
            pages_fetched: run.pages_fetched,
            total_results: results.len(),
            results: raw,
            partial: run.pages_fetched < req.pages,
            errors: failures,
        })
    }

    /// Walk the fallback chain until an engine returns results.
    async fn run_auto(
        &self,
        req: &SearchRequest,
    ) -> Result<(EngineRun, Vec<EngineKind>, Vec<EngineFailure>), AppError> {
        let mut tried = Vec::new();
        let mut failures = Vec::new();
        let mut empty: Option<EngineRun> = None;

        let chain = self.cfg.default_engine.into_iter().chain(
            self.cfg
                .fallback_engines
                .iter()
                .copied()
                .filter(|e| Some(*e) != self.cfg.default_engine),
        );
        for kind in chain {
            let engine = self.engines.get(kind);
            // Skip engines that can't currently succeed without spending a request.
            let browser_ok = self.browser.is_enabled() && req.mode != FetchMode::Http;
            let http_ok = req.mode != FetchMode::Browser && !self.http_cooling(kind) && !engine.requires_js();
            if !browser_ok && !http_ok {
                failures.push(EngineFailure {
                    engine: kind,
                    page: None,
                    error: "skipped: needs the headless browser or is cooling down after a block".into(),
                    blocked: self.http_cooling(kind),
                });
                continue;
            }
            tried.push(kind);
            let run = self.run_engine(kind, req).await;
            if !run.results.is_empty() {
                return Ok((run, tried, failures));
            }
            if run.pages_fetched > 0 {
                empty.get_or_insert(run);
            } else {
                failures.extend(run.failures);
            }
        }
        match empty {
            Some(run) => Ok((run, tried, failures)),
            None => Err(all_failed(failures)),
        }
    }

    /// Fetch all requested pages of one engine concurrently and merge them.
    async fn run_engine(&self, kind: EngineKind, req: &SearchRequest) -> EngineRun {
        let engine = self.engines.get(kind);
        let pages = join_all((0..req.pages).map(|page| async move {
            let q = PageQuery {
                query: &req.query,
                page,
                region: req.region,
                safe: req.safe,
            };
            (page, self.fetch_page(engine, &q, req.mode).await)
        }))
        .await;

        let mut seen = HashSet::new();
        let mut results = Vec::with_capacity(pages.len() * 10);
        let mut failures = Vec::new();
        let mut pages_fetched = 0;
        for (page, outcome) in pages {
            match outcome {
                Ok(raw) => {
                    pages_fetched += 1;
                    for r in raw {
                        if !seen.insert(dedupe_key(&r.url)) {
                            continue;
                        }
                        results.push(SearchResult {
                            position: results.len() as u32 + 1,
                            page: page + 1,
                            engine: kind,
                            domain: domain_of(&r.url),
                            title: r.title,
                            url: r.url,
                            snippet: r.snippet,
                            content: None,
                        });
                    }
                }
                Err(e) => failures.push(EngineFailure {
                    engine: kind,
                    page: Some(page + 1),
                    blocked: e.is_blocked(),
                    error: e.to_string(),
                }),
            }
        }
        EngineRun {
            engine: kind,
            pages_fetched,
            results,
            failures,
        }
    }

    async fn fetch_page(
        &self,
        engine: &dyn Engine,
        q: &PageQuery<'_>,
        mode: FetchMode,
    ) -> Result<Vec<RawResult>, UpstreamError> {
        let kind = engine.kind();
        let browser = self.browser.is_enabled();
        let browser_first = match mode {
            FetchMode::Browser => true,
            FetchMode::Http => false,
            FetchMode::Auto => browser && (engine.requires_js() || self.http_cooling(kind)),
        };
        if browser_first {
            return self.fetch_browser(engine, q).await;
        }
        let http_err = match self.fetch_http(engine, q).await {
            Ok(results) => return Ok(results),
            Err(e) if mode == FetchMode::Auto && browser => e,
            Err(e) => return Err(e),
        };
        tracing::debug!(engine = %kind, error = %http_err, "http path failed, falling back to browser");
        let results = self.fetch_browser(engine, q).await?;
        // The browser works where HTTP doesn't: skip the doomed HTTP attempt for a while.
        if !http_err.is_blocked() {
            self.set_cooldown(kind);
        }
        Ok(results)
    }

    async fn fetch_http(&self, engine: &dyn Engine, q: &PageQuery<'_>) -> Result<Vec<RawResult>, UpstreamError> {
        let kind = engine.kind();
        let req = engine.http_request(q);
        let fetched = {
            let _permit = self.upstream.acquire().await.expect("semaphore never closed");
            self.metrics.upstream_http(kind);
            self.http.fetch_serp(kind, &req).await
        }
        .inspect_err(|_| self.metrics.upstream_error(kind))?;

        if engine.is_blocked(fetched.status, &fetched.body) {
            self.mark_blocked(kind);
            return Err(UpstreamError::Blocked(kind));
        }
        if fetched.status >= 400 {
            self.metrics.upstream_error(kind);
            return Err(UpstreamError::Status {
                engine: kind,
                status: fetched.status,
            });
        }
        let results = engine.parse(&fetched.body);
        if looks_poisoned(q.query, &results) {
            self.mark_blocked(kind);
            return Err(UpstreamError::Blocked(kind));
        }
        Ok(results)
    }

    async fn fetch_browser(&self, engine: &dyn Engine, q: &PageQuery<'_>) -> Result<Vec<RawResult>, UpstreamError> {
        let kind = engine.kind();
        if !self.browser.is_enabled() {
            return Err(UpstreamError::BrowserUnavailable(
                "headless browser is disabled or not compiled in".into(),
            ));
        }
        let url = engine.browser_url(q).ok_or_else(|| UpstreamError::Browser {
            engine: kind,
            message: format!(
                "page {} can't be rendered in the browser (form-POST pagination)",
                q.page + 1
            ),
        })?;
        self.metrics.upstream_browser(kind);
        let html = self
            .browser
            .render(&url, engine.ready_selector())
            .await
            .map_err(|message| {
                self.metrics.upstream_error(kind);
                UpstreamError::Browser { engine: kind, message }
            })?;
        if engine.is_blocked(200, &html) {
            self.metrics.upstream_blocked(kind);
            return Err(UpstreamError::Blocked(kind));
        }
        let results = engine.parse(&html);
        if looks_poisoned(q.query, &results) {
            self.metrics.upstream_blocked(kind);
            return Err(UpstreamError::Blocked(kind));
        }
        Ok(results)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn http_cooling(&self, kind: EngineKind) -> bool {
        self.http_blocked_until[kind.index()].load(Ordering::Relaxed) > self.now_ms()
    }

    fn set_cooldown(&self, kind: EngineKind) {
        let until = self.now_ms() + self.cfg.block_cooldown.as_millis() as u64;
        self.http_blocked_until[kind.index()].store(until, Ordering::Relaxed);
    }

    fn mark_blocked(&self, kind: EngineKind) {
        self.metrics.upstream_blocked(kind);
        self.set_cooldown(kind);
        tracing::warn!(engine = %kind, cooldown_secs = self.cfg.block_cooldown.as_secs(), "engine blocked HTTP request");
    }

    async fn attach_content(&self, results: &mut [SearchResult], limit: usize) {
        let max_chars = self.cfg.content_max_chars;
        let targets: Vec<(usize, String)> = results
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, r)| (i, r.url.clone()))
            .collect();
        let fetched: Vec<(usize, Option<String>)> = stream::iter(targets)
            .map(|(i, url)| async move {
                let _permit = self.content_permits.acquire().await.expect("semaphore never closed");
                Metrics::inc(&self.metrics.content_fetches);
                let text = match self.http.fetch_document(&url).await {
                    Ok(doc) if doc.status < 400 && is_textual(doc.content_type.as_deref()) => {
                        Some(content::extract_text(&doc.body, max_chars))
                    }
                    Ok(_) => None,
                    Err(e) => {
                        tracing::debug!(%url, error = %e, "content fetch failed");
                        None
                    }
                };
                (i, text)
            })
            .buffer_unordered(limit.max(1))
            .collect()
            .await;
        for (i, text) in fetched {
            results[i].content = text.filter(|t| !t.is_empty());
        }
    }
}

/// Words too common to signal relevance.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "how", "what", "with", "are", "you", "who", "why", "when", "where", "which", "does", "can",
    "from", "that", "this", "your", "into", "about", "was", "were", "has", "have", "not",
];

/// Soft-block detection. Some engines (notably Bing) answer suspected bots with
/// a normal-looking SERP of unrelated popular pages that at best echo a single
/// query word. A page is treated as poisoned when no result mentions at least
/// two distinct query terms (one for single-term queries).
fn looks_poisoned(query: &str, results: &[RawResult]) -> bool {
    if results.len() < 3 {
        return false;
    }
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect();
    terms.sort_unstable();
    terms.dedup();
    if terms.is_empty() {
        return false;
    }
    let need = terms.len().min(2);
    !results.iter().any(|r| {
        let hay = format!("{} {} {}", r.title, r.snippet, r.url).to_lowercase();
        let words: HashSet<&str> = hay
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        terms.iter().filter(|t| term_matches(t, &hay, &words)).count() >= need
    })
}

/// ASCII terms must match a whole word (or its plural) so "web" doesn't match
/// "webster"; other scripts (often written without spaces) match as substrings.
fn term_matches(term: &str, hay: &str, words: &HashSet<&str>) -> bool {
    if !term.is_ascii() {
        return hay.contains(term);
    }
    words.contains(term)
        || words
            .iter()
            .any(|w| w.strip_prefix(term).is_some_and(|rest| rest == "s" || rest == "es"))
}

fn is_textual(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.contains("html") || ct.starts_with("text/")
    })
}

/// Normalised URL used to de-duplicate results across pages.
fn dedupe_key(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            let host = u.host_str().unwrap_or_default().trim_start_matches("www.").to_owned();
            let path = u.path().trim_end_matches('/');
            match u.query() {
                Some(q) => format!("{host}{path}?{q}"),
                None => format!("{host}{path}"),
            }
        }
        Err(_) => url.to_owned(),
    }
}

fn all_failed(failures: Vec<EngineFailure>) -> AppError {
    let blocked = failures.iter().any(|f| f.blocked);
    AppError::AllEnginesFailed { blocked, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_normalises() {
        assert_eq!(
            dedupe_key("https://www.rust-lang.org/"),
            dedupe_key("http://rust-lang.org")
        );
        assert_eq!(dedupe_key("https://a.com/x#frag"), dedupe_key("https://a.com/x/"));
        assert_ne!(dedupe_key("https://a.com/x?p=1"), dedupe_key("https://a.com/x?p=2"));
    }

    #[test]
    fn detects_poisoned_serps() {
        let r = |t: &str| RawResult {
            title: t.into(),
            url: "https://example.com/".into(),
            snippet: String::new(),
        };
        let junk = [r("WhatsApp"), r("Facebook"), r("Instagram")];
        assert!(looks_poisoned("tokio runtime", &junk));
        let good = [r("WhatsApp"), r("Tokio - async runtime"), r("Instagram")];
        assert!(!looks_poisoned("tokio runtime", &good));
        // Echoing a single word of a multi-word query is still junk.
        let echo = [r("Best Buy"), r("BEST definition"), r("Best Western")];
        assert!(looks_poisoned("best rust web framework", &echo));
        // Substrings of longer words don't count ("web" in "merriam-webster").
        let webster = [r("BEST Definition - Merriam-Webster"), r("Best Buy"), r("x")];
        assert!(looks_poisoned("best rust web framework", &webster));
        assert!(!looks_poisoned(
            "rust framework",
            &[r("Rust web frameworks"), r("x"), r("y")]
        ));
        // Unsegmented scripts fall back to substring matching.
        assert!(!looks_poisoned(
            "東京都 天気予報",
            &[r("東京都の天気予報 - 気象庁"), r("x"), r("y")]
        ));
        let real = [r("Best Buy"), r("Actix: a Rust web framework"), r("x")];
        assert!(!looks_poisoned("best rust web framework", &real));
        assert!(looks_poisoned(
            "how to bake bread",
            &[r("How to"), r("How are you"), r("The how")]
        ));
        // Too few results or only short terms: don't guess.
        assert!(!looks_poisoned("tokio", &junk[..2]));
        assert!(!looks_poisoned("c++ go", &junk));
        // Case- and script-insensitive.
        assert!(!looks_poisoned("ÜBER Straße", &[r("über die straße"), r("x"), r("y")]));
        assert!(!looks_poisoned("tokio", &[r("x"), r("TOKIO docs"), r("y")]));
    }

    #[test]
    fn textual_types() {
        assert!(is_textual(None));
        assert!(is_textual(Some("text/html; charset=utf-8")));
        assert!(is_textual(Some("application/xhtml+xml")));
        assert!(!is_textual(Some("application/pdf")));
        assert!(!is_textual(Some("image/png")));
    }
}
