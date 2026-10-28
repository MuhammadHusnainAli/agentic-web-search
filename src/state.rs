//! Shared application state, built once at startup and shared via `Arc`.

use std::{collections::HashSet, net::IpAddr, num::NonZeroU32, sync::Arc};

use governor::{DefaultKeyedRateLimiter, Quota};
use sha2::{Digest, Sha256};

use crate::{config::Config, error::AppError, metrics::Metrics, search::SearchService};

/// Identity a request is rate limited under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKey {
    /// Truncated SHA-256 of the API key (never the key itself).
    ApiKey([u8; 16]),
    Ip(IpAddr),
    Anonymous,
}

/// API keys stored as SHA-256 digests: the plaintext never stays in memory
/// after startup, and lookups don't leak key prefixes through timing.
pub struct ApiKeys {
    digests: HashSet<[u8; 32]>,
}

impl ApiKeys {
    pub fn new<'a>(keys: impl IntoIterator<Item = &'a String>) -> Self {
        Self {
            digests: keys.into_iter().map(|k| Self::digest(k)).collect(),
        }
    }

    fn digest(key: &str) -> [u8; 32] {
        Sha256::digest(key.as_bytes()).into()
    }

    /// Returns the client identity for a valid key.
    pub fn verify(&self, key: &str) -> Option<ClientKey> {
        let d = Self::digest(key);
        self.digests.contains(&d).then(|| {
            let mut id = [0u8; 16];
            id.copy_from_slice(&d[..16]);
            ClientKey::ApiKey(id)
        })
    }
}

pub struct AppState {
    pub cfg: Arc<Config>,
    pub search: SearchService,
    pub metrics: Arc<Metrics>,
    pub api_keys: ApiKeys,
    pub limiter: DefaultKeyedRateLimiter<ClientKey>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(cfg: Config) -> Result<SharedState, AppError> {
        let cfg = Arc::new(cfg);
        let metrics = Arc::new(Metrics::default());
        let search = SearchService::new(cfg.clone(), metrics.clone())?;
        let per_sec = NonZeroU32::new(cfg.rate_limit_per_sec).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(cfg.rate_limit_burst).unwrap_or(per_sec);
        let limiter = DefaultKeyedRateLimiter::dashmap(Quota::per_second(per_sec).allow_burst(burst));
        Ok(Arc::new(AppState {
            api_keys: ApiKeys::new(&cfg.api_keys),
            search,
            metrics,
            limiter,
            cfg,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_keys_by_digest() {
        let keys = ApiKeys::new(&["a-very-long-secret-key".to_owned()]);
        assert!(matches!(
            keys.verify("a-very-long-secret-key"),
            Some(ClientKey::ApiKey(_))
        ));
        assert!(keys.verify("a-very-long-secret-kez").is_none());
        assert!(keys.verify("").is_none());
    }
}
