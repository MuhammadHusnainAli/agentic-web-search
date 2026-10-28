//! Pooled HTTP client for SERPs and result pages.

use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use reqwest::{
    Client, Response,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{self, HeaderMap, HeaderValue},
    redirect,
};
use url::Url;

use crate::{config::Config, engines::UpstreamRequest, error::UpstreamError, model::EngineKind};

/// A fetched, size-capped, UTF-8 decoded document.
#[derive(Debug)]
pub struct Fetched {
    /// Final URL after redirects.
    pub url: Url,
    pub status: u16,
    pub body: String,
    pub content_type: Option<String>,
}

pub struct HttpFetcher {
    /// SERP client: talks only to configured engine hosts.
    client: Client,
    /// Result-page client: refuses private / loopback targets (SSRF guard).
    content_client: Client,
    user_agents: Box<[HeaderValue]>,
    next_ua: AtomicUsize,
    max_body: usize,
    allow_private: bool,
}

/// Is `ip` routable on the public internet?
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // 100.64.0.0/10 CGNAT
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80) // link local
        }
    }
}

/// DNS resolver that drops non-public addresses, defeating DNS-rebinding SSRF.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} does not resolve to a public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn host_is_forbidden(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !is_public_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d.eq_ignore_ascii_case("localhost") || d.ends_with(".localhost") || d.ends_with(".internal")
        }
        None => true,
    }
}

impl HttpFetcher {
    pub fn new(cfg: &Config) -> Result<Self, reqwest::Error> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        );
        headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        headers.insert(header::UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"));

        let builder = || {
            let mut b = Client::builder()
                .default_headers(headers.clone())
                .timeout(cfg.upstream_timeout)
                .connect_timeout(Duration::from_secs(5).min(cfg.upstream_timeout))
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(64)
                .tcp_nodelay(true)
                .tcp_keepalive(Duration::from_secs(60))
                .http2_adaptive_window(true);
            if let Some(proxy) = &cfg.proxy_url {
                b = b.proxy(reqwest::Proxy::all(proxy)?);
            }
            Ok::<_, reqwest::Error>(b)
        };

        let client = builder()?.redirect(redirect::Policy::limited(5)).build()?;

        let allow_private = cfg.content_allow_private_ips;
        let mut content = builder()?.redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if !allow_private && host_is_forbidden(attempt.url()) {
                attempt.error("redirect to a non-public address")
            } else {
                attempt.follow()
            }
        }));
        if !allow_private {
            content = content.dns_resolver(PublicOnlyResolver);
        }

        let user_agents = cfg
            .user_agents
            .iter()
            .filter_map(|ua| HeaderValue::from_str(ua).ok())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok(Self {
            client,
            content_client: content.build()?,
            user_agents,
            next_ua: AtomicUsize::new(0),
            max_body: cfg.max_body_bytes,
            allow_private,
        })
    }

    /// Round-robin user agent.
    pub fn user_agent(&self) -> &HeaderValue {
        let i = self.next_ua.fetch_add(1, Ordering::Relaxed);
        &self.user_agents[i % self.user_agents.len()]
    }

    /// Fetch one SERP page.
    pub async fn fetch_serp(&self, engine: EngineKind, req: &UpstreamRequest) -> Result<Fetched, UpstreamError> {
        let mut rb = match &req.form {
            Some(form) => self.client.post(req.url.clone()).form(form),
            None => self.client.get(req.url.clone()),
        };
        rb = rb.header(header::USER_AGENT, self.user_agent());
        for (name, value) in &req.headers {
            rb = rb.header(*name, value);
        }
        let map_err = |e: reqwest::Error| {
            if e.is_timeout() {
                UpstreamError::Timeout { engine }
            } else {
                UpstreamError::Transport {
                    engine,
                    message: e.without_url().to_string(),
                }
            }
        };
        let resp = rb.send().await.map_err(map_err)?;
        read_capped(resp, self.max_body).await.map_err(map_err)
    }

    /// Fetch an arbitrary result page for content extraction (SSRF-guarded).
    pub async fn fetch_document(&self, url: &str) -> Result<Fetched, String> {
        let url = Url::parse(url).map_err(|e| e.to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("unsupported scheme".into());
        }
        if !self.allow_private && host_is_forbidden(&url) {
            return Err("refusing to fetch a non-public address".into());
        }
        let resp = self
            .content_client
            .get(url)
            .header(header::USER_AGENT, self.user_agent())
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        read_capped(resp, self.max_body)
            .await
            .map_err(|e| e.without_url().to_string())
    }
}

/// Stream the body into memory, stopping at `max` bytes so a huge or hostile
/// response can never balloon memory.
async fn read_capped(mut resp: Response, max: usize) -> Result<Fetched, reqwest::Error> {
    let url = resp.url().clone();
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let hint = resp.content_length().unwrap_or(64 * 1024) as usize;
    let mut buf = Vec::with_capacity(hint.min(max));
    while let Some(chunk) = resp.chunk().await? {
        let room = max - buf.len();
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    let body = match String::from_utf8(buf) {
        Ok(s) => s,
        // Truncation may split a multi-byte char; decode lossily instead of failing.
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(Fetched {
        url,
        status,
        body,
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ips() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "0.0.0.0",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn forbids_local_hosts() {
        for u in [
            "http://localhost/",
            "http://127.0.0.1:8080/",
            "http://[::1]/",
            "http://metadata.google.internal/",
            "http://169.254.169.254/latest",
        ] {
            assert!(host_is_forbidden(&Url::parse(u).unwrap()), "{u}");
        }
        assert!(!host_is_forbidden(&Url::parse("https://rust-lang.org/").unwrap()));
    }
}
