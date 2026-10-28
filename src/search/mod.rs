//! Search orchestration: caching, page fan-out, engine fallback, merging and
//! optional content extraction.

pub mod content;

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{StreamExt, future::join_all, stream, stream::FuturesUnordered};
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
    model::{
        ContentFormat, ContentMode, EngineChoice, EngineKind, FetchMode, RawResult, Region, SafeSearch, SearchResult,
        Strategy, TimeRange,
    },
};

/// A validated search request. Also the cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchRequest {
    pub query: String,
    pub engine: EngineChoice,
    /// Chain override for `engine=auto`; empty = configured fallback chain.
    pub engines: Vec<EngineKind>,
    pub strategy: Strategy,
    pub pages: u32,
    pub mode: FetchMode,
    pub region: Region,
    pub safe: SafeSearch,
    pub time_range: Option<TimeRange>,
    /// Sorted, de-duplicated bare hosts.
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub file_type: Option<String>,
    pub unique_domains: bool,
    pub max_results: Option<u32>,
    pub include_content: bool,
    pub content_limit: u32,
    pub content_format: ContentFormat,
    pub content_mode: ContentMode,
    pub content_max_chars: u32,
    pub content_timeout_ms: u32,
}

impl SearchRequest {
    /// The query sent upstream: the caller's query plus the search operators
    /// that express its filters. Results are post-filtered as well, since
    /// engines don't always honour operators.
    pub fn upstream_query(&self) -> String {
        let mut q = self.query.clone();
        match self.include_domains.as_slice() {
            [] => {}
            [d] => {
                let _ = write!(q, " site:{d}");
            }
            many => {
                let sites: Vec<String> = many.iter().map(|d| format!("site:{d}")).collect();
                let _ = write!(q, " ({})", sites.join(" OR "));
            }
        }
        for d in &self.exclude_domains {
            let _ = write!(q, " -site:{d}");
        }
        if let Some(ft) = &self.file_type {
            let _ = write!(q, " filetype:{ft}");
        }
        q
    }

    fn keeps_domain(&self, domain: &str) -> bool {
        let matches = |d: &String| domain == d || domain.strip_suffix(d.as_str()).is_some_and(|p| p.ends_with('.'));
        (self.include_domains.is_empty() || self.include_domains.iter().any(matches))
            && !self.exclude_domains.iter().any(matches)
    }
}

/// The cacheable part of a search response. Results are stored pre-serialised
/// so cache hits never re-encode them.
#[derive(Debug, Serialize)]
pub struct SearchOutput {
    pub engine: EngineKind,
    pub engines_tried: Vec<EngineKind>,
    pub engines_used: Vec<EngineKind>,
    pub pages_fetched: u32,
    pub total_results: usize,
    pub results: Box<RawValue>,
    pub errors: Vec<EngineFailure>,
    pub warnings: Vec<String>,
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

/// Engine runs that produced a SERP, best first, plus everything that didn't.
struct Outcome {
    runs: Vec<EngineRun>,
    tried: Vec<EngineKind>,
    failures: Vec<EngineFailure>,
}

pub struct SearchService {
    cfg: Arc<Config>,
    engines: Engines,
    http: HttpFetcher,
    browser: Arc<BrowserPool>,
    cache: Cache<SearchRequest, Arc<SearchOutput>>,
    /// Extracted pages by URL, shared across searches.
    content_cache: Cache<String, Arc<content::Page>>,
    upstream: Semaphore,
    content_permits: Semaphore,
    /// Caps CPU-bound extraction at the core count: more parse threads than
    /// cores adds no throughput, only per-thread allocator heaps.
    parse_permits: Semaphore,
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
                let bytes = k.query.len()
                    + v.results.get().len()
                    + 256
                    + v.errors.len() * 128
                    + v.warnings.iter().map(String::len).sum::<usize>();
                u32::try_from(bytes).unwrap_or(u32::MAX)
            })
            .expire_after(OutputExpiry { ttl: cfg.cache_ttl })
            .build();
        let content_cache = Cache::builder()
            .max_capacity(cfg.content_cache_max_bytes)
            .weigher(|k: &String, v: &Arc<content::Page>| u32::try_from(k.len() + v.weight()).unwrap_or(u32::MAX))
            .time_to_live(cfg.cache_ttl)
            .build();
        Ok(Self {
            engines: Engines::new(&cfg),
            http,
            browser,
            cache,
            content_cache,
            upstream: Semaphore::new(cfg.max_upstream_concurrency),
            content_permits: Semaphore::new(cfg.content_max_concurrency),
            parse_permits: Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get())),
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
    /// fetch. Returns the output and whether it was served from cache. With
    /// `fresh`, the cache is bypassed (the new result still replaces it).
    pub async fn search(&self, req: &SearchRequest, fresh: bool) -> Result<(Arc<SearchOutput>, bool), AppError> {
        if fresh {
            Metrics::inc(&self.metrics.cache_misses);
            let out = Arc::new(
                self.execute(req)
                    .await
                    .inspect_err(|_| Metrics::inc(&self.metrics.search_failures))?,
            );
            self.cache.insert(req.clone(), out.clone()).await;
            return Ok((out, false));
        }
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
        let query = req.upstream_query();
        let Outcome {
            mut runs,
            tried,
            mut failures,
        } = match req.engine {
            EngineChoice::Engine(kind) => {
                let run = self.run_engine(kind, req, &query).await;
                if run.pages_fetched == 0 {
                    return Err(all_failed(run.failures));
                }
                Outcome {
                    runs: vec![run],
                    tried: vec![kind],
                    failures: Vec::new(),
                }
            }
            EngineChoice::Auto => match req.strategy {
                Strategy::Fallback => self.run_hedged(req, &query, self.cfg.hedge_delay).await?,
                Strategy::Race => self.run_hedged(req, &query, Duration::ZERO).await?,
                Strategy::Merge => self.run_merge(req, &query).await?,
            },
        };

        let engine = runs[0].engine;
        let pages_fetched = runs.iter().map(|r| r.pages_fetched).max().unwrap_or(0);
        let partial = runs.iter().any(|r| r.pages_fetched < req.pages);
        let mut engines_used: Vec<EngineKind> = runs
            .iter()
            .filter(|r| !r.results.is_empty())
            .map(|r| r.engine)
            .collect();
        if engines_used.is_empty() {
            engines_used.push(engine);
        }
        for run in &mut runs {
            failures.append(&mut run.failures);
        }

        let mut results = if runs.len() == 1 {
            std::mem::take(&mut runs[0].results)
        } else {
            fuse(&mut runs)
        };
        if req.unique_domains {
            let mut seen = HashSet::new();
            results.retain(|r| seen.insert(r.domain.clone()));
        }
        if let Some(max) = req.max_results {
            results.truncate(max as usize);
        }
        for (i, r) in results.iter_mut().enumerate() {
            r.position = i as u32 + 1;
        }
        if req.include_content {
            self.attach_content(&mut results, req).await;
        }

        let warnings = req
            .time_range
            .map(|t| {
                engines_used
                    .iter()
                    .filter(|k| !self.engines.get(**k).supports_time_range(t))
                    .map(|k| format!("{k} has no time_range filter; its results are not date-restricted"))
                    .collect()
            })
            .unwrap_or_default();

        let raw = serde_json::value::to_raw_value(&results).map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(SearchOutput {
            engine,
            engines_tried: tried,
            engines_used,
            pages_fetched,
            total_results: results.len(),
            results: raw,
            partial,
            errors: failures,
            warnings,
        })
    }

    /// Engines `auto` walks, in priority order.
    fn chain(&self, req: &SearchRequest) -> Vec<EngineKind> {
        if !req.engines.is_empty() {
            return req.engines.clone();
        }
        self.cfg
            .default_engine
            .into_iter()
            .chain(
                self.cfg
                    .fallback_engines
                    .iter()
                    .copied()
                    .filter(|e| Some(*e) != self.cfg.default_engine),
            )
            .collect()
    }

    /// Next engine in `chain` that can currently succeed, recording the ones
    /// skipped without spending a request.
    fn next_engine(
        &self,
        chain: &mut impl Iterator<Item = EngineKind>,
        req: &SearchRequest,
        failures: &mut Vec<EngineFailure>,
    ) -> Option<EngineKind> {
        for kind in chain {
            let browser_ok = self.browser.is_enabled() && req.mode != FetchMode::Http;
            let http_ok =
                req.mode != FetchMode::Browser && !self.http_cooling(kind) && !self.engines.get(kind).requires_js();
            if browser_ok || http_ok {
                return Some(kind);
            }
            failures.push(EngineFailure {
                engine: kind,
                page: None,
                error: "skipped: needs the headless browser or is cooling down after a block".into(),
                blocked: self.http_cooling(kind),
            });
        }
        None
    }

    /// Walk the chain; the first engine with results wins. An engine that
    /// fails is replaced immediately, and one that is merely slow gets company
    /// after `hedge` (zero = start every engine at once). Losers are dropped,
    /// which cancels their in-flight requests.
    async fn run_hedged(&self, req: &SearchRequest, query: &str, hedge: Duration) -> Result<Outcome, AppError> {
        let mut chain = self.chain(req).into_iter();
        let mut tried = Vec::new();
        let mut failures = Vec::new();
        let mut empty: Option<EngineRun> = None;
        let mut inflight = FuturesUnordered::new();
        let mut exhausted = false;

        loop {
            let want = if hedge.is_zero() { usize::MAX } else { 1 };
            while !exhausted && inflight.len() < want {
                match self.next_engine(&mut chain, req, &mut failures) {
                    Some(kind) => {
                        tried.push(kind);
                        inflight.push(self.run_engine(kind, req, query));
                    }
                    None => exhausted = true,
                }
            }
            if inflight.is_empty() {
                break;
            }
            tokio::select! {
                Some(run) = inflight.next() => {
                    if !run.results.is_empty() {
                        return Ok(Outcome { runs: vec![run], tried, failures });
                    }
                    if run.pages_fetched > 0 {
                        empty.get_or_insert(run);
                    } else {
                        failures.extend(run.failures);
                    }
                }
                _ = tokio::time::sleep(hedge), if !hedge.is_zero() && !exhausted => {
                    if let Some(kind) = self.next_engine(&mut chain, req, &mut failures) {
                        tracing::debug!(engine = %kind, "hedging slow engine");
                        Metrics::inc(&self.metrics.hedged);
                        tried.push(kind);
                        inflight.push(self.run_engine(kind, req, query));
                    } else {
                        exhausted = true;
                    }
                }
            }
        }
        match empty {
            Some(run) => Ok(Outcome {
                runs: vec![run],
                tried,
                failures,
            }),
            None => Err(all_failed(failures)),
        }
    }

    /// Query every engine concurrently. Once the first one returns results,
    /// stragglers get `merge_grace` to finish before the rankings are fused.
    async fn run_merge(&self, req: &SearchRequest, query: &str) -> Result<Outcome, AppError> {
        let mut chain = self.chain(req).into_iter();
        let mut tried = Vec::new();
        let mut failures = Vec::new();
        let mut inflight = FuturesUnordered::new();
        while let Some(kind) = self.next_engine(&mut chain, req, &mut failures) {
            tried.push(kind);
            inflight.push(self.run_engine(kind, req, query));
        }

        let mut runs = Vec::new();
        let mut done = HashSet::new();
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            let next = match deadline {
                None => inflight.next().await,
                Some(d) => match tokio::time::timeout_at(d, inflight.next()).await {
                    Ok(next) => next,
                    Err(_) => break,
                },
            };
            let Some(run) = next else { break };
            done.insert(run.engine);
            if run.pages_fetched == 0 {
                failures.extend(run.failures);
                continue;
            }
            if deadline.is_none() && !run.results.is_empty() {
                deadline = Some(tokio::time::Instant::now() + self.cfg.merge_grace);
            }
            runs.push(run);
        }
        for kind in tried.iter().filter(|k| !done.contains(*k)) {
            failures.push(EngineFailure {
                engine: *kind,
                page: None,
                error: format!(
                    "{kind} was still running {} ms after the first engine answered; dropped",
                    self.cfg.merge_grace.as_millis()
                ),
                blocked: false,
            });
        }
        if runs.is_empty() {
            return Err(all_failed(failures));
        }
        // Chain priority breaks ranking ties.
        runs.sort_by_key(|r| tried.iter().position(|k| *k == r.engine));
        Ok(Outcome { runs, tried, failures })
    }

    /// Fetch all requested pages of one engine concurrently and merge them.
    async fn run_engine(&self, kind: EngineKind, req: &SearchRequest, query: &str) -> EngineRun {
        let engine = self.engines.get(kind);
        let pages = join_all((0..req.pages).map(|page| async move {
            let q = PageQuery {
                query,
                page,
                region: req.region,
                safe: req.safe,
                time_range: req.time_range,
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
                        let domain = domain_of(&r.url);
                        if !req.keeps_domain(&domain) || !seen.insert(dedupe_key(&r.url)) {
                            continue;
                        }
                        results.push(SearchResult {
                            position: results.len() as u32 + 1,
                            page: page + 1,
                            engine: kind,
                            domain,
                            title: r.title,
                            url: r.url,
                            snippet: r.snippet,
                            engines: Vec::new(),
                            content: None,
                            content_error: None,
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
        // Rendered on its own task: when a raced/hedged search drops this
        // future, the render still finishes and closes its tab.
        let pool = self.browser.clone();
        let selector = engine.ready_selector();
        let html = tokio::spawn(async move { pool.render(&url, selector).await })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
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

    /// Attach extracted page text to the top results. Everything must finish
    /// within the request's content deadline; slower pages get `content_error`.
    async fn attach_content(&self, results: &mut [SearchResult], req: &SearchRequest) {
        let limit = req.content_limit as usize;
        let (format, max_chars) = (req.content_format, req.content_max_chars as usize);
        let query = &req.query;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(req.content_timeout_ms));
        let targets: Vec<(usize, String)> = results
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, r)| (i, r.url.clone()))
            .collect();
        let fetched: Vec<(usize, Result<String, String>)> = stream::iter(targets)
            .map(|(i, url)| async move {
                let text = match tokio::time::timeout_at(deadline, self.page(&url)).await {
                    Ok(Ok(page)) => Ok(match req.content_mode {
                        ContentMode::Full => page.render(format, max_chars).0,
                        ContentMode::Relevant => page.render_relevant(query, format, max_chars).0,
                    }),
                    Ok(Err(e)) => {
                        tracing::debug!(%url, error = %e, "content fetch failed");
                        Err(e)
                    }
                    Err(_) => {
                        Metrics::inc(&self.metrics.content_timeouts);
                        Err(format!("timed out after {} ms", req.content_timeout_ms))
                    }
                };
                (i, text)
            })
            .buffer_unordered(limit.max(1))
            .collect()
            .await;
        for (i, text) in fetched {
            match text {
                Ok(t) if !t.is_empty() => results[i].content = Some(t),
                Ok(_) => results[i].content_error = Some("no readable text".into()),
                Err(e) => results[i].content_error = Some(e),
            }
        }
    }

    /// Readable content of `url` (up to `CONTENT_MAX_CHARS`), cached and
    /// coalesced across searches. Parsing runs on the blocking pool so large
    /// pages never stall the async workers.
    async fn page(&self, url: &str) -> Result<Arc<content::Page>, String> {
        self.content_cache
            .try_get_with_by_ref(url, async {
                let _permit = self.content_permits.acquire().await.expect("semaphore never closed");
                Metrics::inc(&self.metrics.content_fetches);
                let doc = self.http.fetch_document(url).await?;
                if doc.status >= 400 {
                    return Err(format!("HTTP {}", doc.status));
                }
                if !is_textual(doc.content_type.as_deref()) {
                    return Err(format!(
                        "unsupported content type {}",
                        doc.content_type.as_deref().unwrap_or_default()
                    ));
                }
                let max = self.cfg.content_max_chars;
                let _parse = self.parse_permits.acquire().await.expect("semaphore never closed");
                let page = tokio::task::spawn_blocking(move || content::extract(&doc.body, Some(&doc.url), max))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Arc::new(page))
            })
            .await
            .map_err(|e: Arc<String>| (*e).clone())
    }
}

/// Reciprocal-rank fusion of several engines' rankings: a URL scores
/// `Σ 1/(60 + rank)` over the engines that returned it, so agreement between
/// engines beats a single high placement. Keeps the longest snippet.
fn fuse(runs: &mut [EngineRun]) -> Vec<SearchResult> {
    const K: f64 = 60.0;
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<(f64, SearchResult)> = Vec::new();
    for run in runs.iter_mut() {
        for (rank, mut r) in run.results.drain(..).enumerate() {
            let score = 1.0 / (K + rank as f64 + 1.0);
            match index.entry(dedupe_key(&r.url)) {
                Entry::Occupied(e) => {
                    let (s, m) = &mut merged[*e.get()];
                    *s += score;
                    m.engines.push(r.engine);
                    if r.snippet.len() > m.snippet.len() {
                        m.snippet = r.snippet;
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(merged.len());
                    r.engines = vec![r.engine];
                    merged.push((score, r));
                }
            }
        }
    }
    // Stable: equal scores keep chain priority.
    merged.sort_by(|a, b| b.0.total_cmp(&a.0));
    merged.into_iter().map(|(_, r)| r).collect()
}

/// Words too common to signal relevance.
pub(crate) const STOPWORDS: &[&str] = &[
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
        .split_whitespace()
        // Operators added for filters (`site:x`, `-site:y`, `filetype:pdf`) aren't topical.
        .filter(|w| !w.contains(':') && !w.starts_with('-'))
        .flat_map(|w| w.split(|c: char| !c.is_alphanumeric()))
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

    fn request() -> SearchRequest {
        SearchRequest {
            query: "tokio runtime".into(),
            engine: EngineChoice::Auto,
            engines: Vec::new(),
            strategy: Strategy::Fallback,
            pages: 1,
            mode: FetchMode::Http,
            region: Region::DEFAULT,
            safe: SafeSearch::Moderate,
            time_range: None,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            file_type: None,
            unique_domains: false,
            max_results: None,
            include_content: false,
            content_limit: 0,
            content_format: ContentFormat::Markdown,
            content_mode: ContentMode::Full,
            content_max_chars: 0,
            content_timeout_ms: 0,
        }
    }

    #[test]
    fn builds_upstream_query_and_filters_domains() {
        let mut r = request();
        assert_eq!(r.upstream_query(), "tokio runtime");
        r.include_domains = vec!["docs.rs".into()];
        r.file_type = Some("pdf".into());
        assert_eq!(r.upstream_query(), "tokio runtime site:docs.rs filetype:pdf");
        assert!(r.keeps_domain("docs.rs") && r.keeps_domain("api.docs.rs"));
        assert!(!r.keeps_domain("notdocs.rs") && !r.keeps_domain("github.com"));
        r.include_domains = vec!["a.com".into(), "b.org".into()];
        r.exclude_domains = vec!["x.a.com".into()];
        r.file_type = None;
        assert_eq!(
            r.upstream_query(),
            "tokio runtime (site:a.com OR site:b.org) -site:x.a.com"
        );
        assert!(r.keeps_domain("a.com") && !r.keeps_domain("x.a.com") && !r.keeps_domain("y.x.a.com"));
        // Operators never count as query terms for the relevance guard.
        let rr = |t: &str| RawResult {
            title: t.into(),
            url: "https://example.com/".into(),
            snippet: String::new(),
        };
        assert!(!looks_poisoned(
            "tokio site:docs.rs -site:github.com",
            &[rr("Tokio docs"), rr("x"), rr("y")]
        ));
    }

    #[test]
    fn fuses_rankings() {
        let res = |engine, url: &str, snippet: &str| SearchResult {
            position: 0,
            page: 1,
            engine,
            title: url.into(),
            url: url.into(),
            domain: domain_of(url),
            snippet: snippet.into(),
            engines: Vec::new(),
            content: None,
            content_error: None,
        };
        let run = |engine, urls: &[(&str, &str)]| EngineRun {
            engine,
            pages_fetched: 1,
            results: urls.iter().map(|(u, s)| res(engine, u, s)).collect(),
            failures: Vec::new(),
        };
        let mut runs = [
            run(EngineKind::Bing, &[("https://a.com/", "a"), ("https://b.com/", "b")]),
            run(
                EngineKind::Brave,
                &[("https://b.com", "longer b snippet"), ("https://c.com/", "c")],
            ),
        ];
        let fused = fuse(&mut runs);
        let urls: Vec<&str> = fused.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(urls, ["b.com", "a.com", "c.com"], "agreement wins");
        assert_eq!(fused[0].engines, [EngineKind::Bing, EngineKind::Brave]);
        assert_eq!(fused[0].snippet, "longer b snippet");
        assert_eq!(fused[1].engines, [EngineKind::Bing]);
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
