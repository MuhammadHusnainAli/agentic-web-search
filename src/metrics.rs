//! Lock-free counters rendered in Prometheus text format.

use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

use crate::model::EngineKind;

const N: usize = EngineKind::ALL.len();

#[derive(Default)]
struct PerEngine([AtomicU64; N]);

impl PerEngine {
    fn inc(&self, e: EngineKind) {
        self.0[e.index()].fetch_add(1, Relaxed);
    }
}

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub unauthorized: AtomicU64,
    pub rate_limited: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub search_failures: AtomicU64,
    pub content_fetches: AtomicU64,
    pub content_timeouts: AtomicU64,
    pub hedged: AtomicU64,
    upstream_http: PerEngine,
    upstream_browser: PerEngine,
    upstream_errors: PerEngine,
    upstream_blocked: PerEngine,
}

impl Metrics {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Relaxed);
    }

    pub fn upstream_http(&self, e: EngineKind) {
        self.upstream_http.inc(e);
    }
    pub fn upstream_browser(&self, e: EngineKind) {
        self.upstream_browser.inc(e);
    }
    pub fn upstream_error(&self, e: EngineKind) {
        self.upstream_errors.inc(e);
    }
    pub fn upstream_blocked(&self, e: EngineKind) {
        self.upstream_blocked.inc(e);
    }

    pub fn render(&self, cache_entries: u64, cache_bytes: u64) -> String {
        let scalars: [(&str, &str, &str, u64); 11] = [
            (
                "aws_requests_total",
                "counter",
                "Search API requests received.",
                self.requests.load(Relaxed),
            ),
            (
                "aws_unauthorized_total",
                "counter",
                "Requests rejected for a missing/invalid API key.",
                self.unauthorized.load(Relaxed),
            ),
            (
                "aws_rate_limited_total",
                "counter",
                "Requests rejected by the rate limiter.",
                self.rate_limited.load(Relaxed),
            ),
            (
                "aws_cache_hits_total",
                "counter",
                "Searches served from cache.",
                self.cache_hits.load(Relaxed),
            ),
            (
                "aws_cache_misses_total",
                "counter",
                "Searches that went upstream.",
                self.cache_misses.load(Relaxed),
            ),
            (
                "aws_search_failures_total",
                "counter",
                "Searches where every engine failed.",
                self.search_failures.load(Relaxed),
            ),
            (
                "aws_content_fetches_total",
                "counter",
                "Result pages fetched for content extraction.",
                self.content_fetches.load(Relaxed),
            ),
            (
                "aws_content_timeouts_total",
                "counter",
                "Result pages whose content missed the request's content deadline.",
                self.content_timeouts.load(Relaxed),
            ),
            (
                "aws_hedged_total",
                "counter",
                "Extra engines started because the current one was slow.",
                self.hedged.load(Relaxed),
            ),
            ("aws_cache_entries", "gauge", "Entries currently cached.", cache_entries),
            (
                "aws_cache_bytes",
                "gauge",
                "Approximate bytes held by the cache.",
                cache_bytes,
            ),
        ];
        let per_engine: [(&str, &str, &PerEngine); 4] = [
            (
                "aws_upstream_http_total",
                "SERP pages fetched over HTTP.",
                &self.upstream_http,
            ),
            (
                "aws_upstream_browser_total",
                "SERP pages rendered in the headless browser.",
                &self.upstream_browser,
            ),
            (
                "aws_upstream_errors_total",
                "SERP page fetch errors.",
                &self.upstream_errors,
            ),
            (
                "aws_upstream_blocked_total",
                "SERP pages that hit a captcha / bot wall.",
                &self.upstream_blocked,
            ),
        ];

        let mut out = String::with_capacity(4096);
        for (name, kind, help, v) in scalars {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}");
        }
        for (name, help, c) in per_engine {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            for e in EngineKind::ALL {
                let _ = writeln!(out, "{name}{{engine=\"{e}\"}} {}", c.0[e.index()].load(Relaxed));
            }
        }
        out
    }
}
