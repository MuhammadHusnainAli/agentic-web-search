//! Typed configuration loaded from environment variables (see `.env.example`).

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use crate::model::{EngineKind, FetchMode, Region};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct BrowserSettings {
    /// Allow the headless browser to be used at all.
    pub enabled: bool,
    /// Path to a Chrome/Chromium binary. Auto-detected when `None`.
    pub executable: Option<PathBuf>,
    /// Max concurrently open tabs (each tab is ~20-60 MB of RAM).
    pub max_tabs: usize,
    /// Per-navigation timeout.
    pub page_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    /// Tokio worker threads; `None` = number of CPU cores.
    pub worker_threads: Option<usize>,
    pub log_format: LogFormat,

    // --- auth & limits ---
    pub require_auth: bool,
    pub api_keys: Vec<String>,
    /// Sustained requests/second allowed per API key (or per client IP when auth is off).
    pub rate_limit_per_sec: u32,
    pub rate_limit_burst: u32,
    /// Max in-flight HTTP requests; above this the service sheds load with 503.
    pub max_concurrent_requests: usize,
    pub request_timeout: Duration,
    pub cors_allow_any: bool,
    /// gzip/brotli responses (fastest level) when the client accepts them.
    pub compression: bool,

    // --- search behaviour ---
    pub default_engine: Option<EngineKind>,
    pub fallback_engines: Vec<EngineKind>,
    pub default_mode: FetchMode,
    pub default_region: Region,
    pub max_pages: u32,
    pub max_query_len: usize,
    /// How long an engine's HTTP path is skipped after it was detected as blocked.
    pub block_cooldown: Duration,

    // --- upstream fetching ---
    pub upstream_timeout: Duration,
    pub max_upstream_concurrency: usize,
    pub max_body_bytes: usize,
    pub proxy_url: Option<String>,
    pub user_agents: Vec<String>,
    /// Per-engine base URL overrides, e.g. for mirrors or tests.
    pub engine_base_urls: HashMap<EngineKind, String>,

    // --- caching ---
    pub cache_ttl: Duration,
    pub cache_max_bytes: u64,

    // --- content extraction ---
    pub content_max_chars: usize,
    pub content_max_concurrency: usize,
    pub content_max_results: usize,
    /// Allow fetching content from private / loopback addresses (SSRF guard off). Tests only.
    pub content_allow_private_ips: bool,

    pub browser: BrowserSettings,
}

pub const DEFAULT_USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
];

impl Default for Config {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 8080,
            worker_threads: None,
            log_format: LogFormat::Pretty,
            require_auth: true,
            api_keys: Vec::new(),
            rate_limit_per_sec: 50,
            rate_limit_burst: 100,
            max_concurrent_requests: 10_000,
            request_timeout: Duration::from_secs(30),
            cors_allow_any: false,
            compression: true,
            default_engine: None,
            fallback_engines: vec![
                EngineKind::Bing,
                EngineKind::Brave,
                EngineKind::DuckDuckGo,
                EngineKind::Yahoo,
                EngineKind::Mojeek,
                EngineKind::Google,
            ],
            default_mode: FetchMode::Auto,
            default_region: Region::DEFAULT,
            max_pages: 5,
            max_query_len: 512,
            block_cooldown: Duration::from_secs(120),
            upstream_timeout: Duration::from_secs(10),
            max_upstream_concurrency: 256,
            max_body_bytes: 3 * 1024 * 1024,
            proxy_url: None,
            user_agents: DEFAULT_USER_AGENTS.iter().map(|s| s.to_string()).collect(),
            engine_base_urls: HashMap::new(),
            cache_ttl: Duration::from_secs(900),
            cache_max_bytes: 256 * 1024 * 1024,
            content_max_chars: 8_000,
            content_max_concurrency: 8,
            content_max_results: 10,
            content_allow_private_ips: false,
            browser: BrowserSettings {
                enabled: cfg!(feature = "browser"),
                executable: None,
                max_tabs: 4,
                page_timeout: Duration::from_secs(20),
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid configuration: {0}")]
pub struct ConfigError(String);

fn var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn parse<T: FromStr>(key: &str, default: T) -> Result<T, ConfigError>
where
    T::Err: std::fmt::Display,
{
    match var(key) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|e| ConfigError(format!("{key}={v}: {e}"))),
    }
}

fn parse_bool(key: &str, default: bool) -> Result<bool, ConfigError> {
    match var(key).as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(v) => Err(ConfigError(format!("{key}={v}: expected true/false"))),
    }
}

fn parse_secs(key: &str, default: Duration) -> Result<Duration, ConfigError> {
    parse::<f64>(key, default.as_secs_f64()).and_then(|s| {
        if s.is_finite() && s > 0.0 {
            Ok(Duration::from_secs_f64(s))
        } else {
            Err(ConfigError(format!("{key}: must be a positive number of seconds")))
        }
    })
}

fn csv(key: &str) -> Option<Vec<String>> {
    var(key).map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

impl Config {
    /// Build configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let d = Config::default();

        let fallback_engines = match csv("FALLBACK_ENGINES") {
            None => d.fallback_engines,
            Some(list) => list
                .iter()
                .map(|s| s.parse::<EngineKind>().map_err(ConfigError))
                .collect::<Result<Vec<_>, _>>()?,
        };

        let default_engine = match var("DEFAULT_ENGINE") {
            None => None,
            Some(v) if v.eq_ignore_ascii_case("auto") => None,
            Some(v) => Some(v.parse::<EngineKind>().map_err(ConfigError)?),
        };

        let default_mode = match var("DEFAULT_FETCH_MODE").as_deref() {
            None | Some("auto") => FetchMode::Auto,
            Some("http") => FetchMode::Http,
            Some("browser") => FetchMode::Browser,
            Some(v) => {
                return Err(ConfigError(format!(
                    "DEFAULT_FETCH_MODE={v}: expected auto|http|browser"
                )));
            }
        };

        let mut engine_base_urls = HashMap::new();
        for kind in EngineKind::ALL {
            let key = format!("ENGINE_{}_URL", kind.as_str().to_ascii_uppercase());
            if let Some(v) = var(&key) {
                url::Url::parse(&v).map_err(|e| ConfigError(format!("{key}={v}: {e}")))?;
                engine_base_urls.insert(kind, v);
            }
        }

        let log_format = match var("LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(v) => return Err(ConfigError(format!("LOG_FORMAT={v}: expected pretty|json"))),
        };

        let worker_threads = match parse::<usize>("WORKER_THREADS", 0)? {
            0 => None,
            n => Some(n),
        };

        let cfg = Config {
            host: parse("HOST", d.host)?,
            port: parse("PORT", d.port)?,
            worker_threads,
            log_format,
            require_auth: parse_bool("REQUIRE_AUTH", d.require_auth)?,
            api_keys: csv("API_KEYS").unwrap_or_default(),
            rate_limit_per_sec: parse("RATE_LIMIT_PER_SEC", d.rate_limit_per_sec)?,
            rate_limit_burst: parse("RATE_LIMIT_BURST", d.rate_limit_burst)?,
            max_concurrent_requests: parse("MAX_CONCURRENT_REQUESTS", d.max_concurrent_requests)?,
            request_timeout: parse_secs("REQUEST_TIMEOUT_SECS", d.request_timeout)?,
            cors_allow_any: parse_bool("CORS_ALLOW_ANY", d.cors_allow_any)?,
            compression: match var("COMPRESSION").as_deref() {
                None | Some("fast") => true,
                Some("off") => false,
                Some(v) => return Err(ConfigError(format!("COMPRESSION={v}: expected fast|off"))),
            },
            default_engine,
            fallback_engines,
            default_mode,
            default_region: parse("DEFAULT_REGION", d.default_region)?,
            max_pages: parse("MAX_PAGES", d.max_pages)?,
            max_query_len: parse("MAX_QUERY_LEN", d.max_query_len)?,
            block_cooldown: parse_secs("BLOCK_COOLDOWN_SECS", d.block_cooldown)?,
            upstream_timeout: parse_secs("UPSTREAM_TIMEOUT_SECS", d.upstream_timeout)?,
            max_upstream_concurrency: parse("MAX_UPSTREAM_CONCURRENCY", d.max_upstream_concurrency)?,
            max_body_bytes: parse("MAX_BODY_BYTES", d.max_body_bytes)?,
            proxy_url: var("PROXY_URL"),
            user_agents: csv("USER_AGENTS").unwrap_or(d.user_agents),
            engine_base_urls,
            cache_ttl: parse_secs("CACHE_TTL_SECS", d.cache_ttl)?,
            cache_max_bytes: parse::<u64>("CACHE_MAX_MB", d.cache_max_bytes / (1024 * 1024))? * 1024 * 1024,
            content_max_chars: parse("CONTENT_MAX_CHARS", d.content_max_chars)?,
            content_max_concurrency: parse("CONTENT_MAX_CONCURRENCY", d.content_max_concurrency)?,
            content_max_results: parse("CONTENT_MAX_RESULTS", d.content_max_results)?,
            content_allow_private_ips: parse_bool("CONTENT_ALLOW_PRIVATE_IPS", d.content_allow_private_ips)?,
            browser: BrowserSettings {
                enabled: parse_bool("BROWSER_ENABLED", d.browser.enabled)?,
                executable: var("BROWSER_EXECUTABLE").map(PathBuf::from),
                max_tabs: parse("BROWSER_MAX_TABS", d.browser.max_tabs)?,
                page_timeout: parse_secs("BROWSER_PAGE_TIMEOUT_SECS", d.browser.page_timeout)?,
            },
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.require_auth && self.api_keys.is_empty() {
            return Err(ConfigError(
                "API_KEYS is empty while REQUIRE_AUTH=true; set API_KEYS=key1,key2 or REQUIRE_AUTH=false".into(),
            ));
        }
        if let Some(k) = self.api_keys.iter().find(|k| k.len() < 16) {
            return Err(ConfigError(format!(
                "API key `{}…` is too short; use at least 16 characters",
                &k[..k.len().min(4)]
            )));
        }
        let checks = [
            (self.max_pages == 0 || self.max_pages > 20, "MAX_PAGES must be 1..=20"),
            (self.rate_limit_per_sec == 0, "RATE_LIMIT_PER_SEC must be > 0"),
            (self.rate_limit_burst == 0, "RATE_LIMIT_BURST must be > 0"),
            (self.max_concurrent_requests == 0, "MAX_CONCURRENT_REQUESTS must be > 0"),
            (
                self.max_upstream_concurrency == 0,
                "MAX_UPSTREAM_CONCURRENCY must be > 0",
            ),
            (self.content_max_concurrency == 0, "CONTENT_MAX_CONCURRENCY must be > 0"),
            (self.browser.max_tabs == 0, "BROWSER_MAX_TABS must be > 0"),
            (self.max_body_bytes < 64 * 1024, "MAX_BODY_BYTES must be >= 65536"),
            (self.user_agents.is_empty(), "USER_AGENTS must not be empty"),
            (self.fallback_engines.is_empty(), "FALLBACK_ENGINES must not be empty"),
        ];
        match checks.iter().find(|(bad, _)| *bad) {
            Some((_, msg)) => Err(ConfigError((*msg).into())),
            None => Ok(()),
        }
    }

    pub fn base_url(&self, kind: EngineKind) -> Option<&str> {
        self.engine_base_urls.get(&kind).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_requires_keys() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
        let cfg = Config {
            api_keys: vec!["0123456789abcdef".into()],
            ..Config::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_short_keys() {
        let cfg = Config {
            api_keys: vec!["short".into()],
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }
}
