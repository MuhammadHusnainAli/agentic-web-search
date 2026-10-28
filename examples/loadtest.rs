//! Minimal HTTP load generator.
//!
//! ```sh
//! cargo run --release --example loadtest -- \
//!   http://127.0.0.1:8080/v1/search?q=rust&engine=brave  <api-key>  <concurrency=256>  <seconds=10>
//! ```
//!
//! Reports throughput, status codes and latency percentiles.
//!
//! - `{n}` in the URL is replaced by a counter per request, so every request
//!   misses the cache (e.g. `?q=rust+{n}`). With `KEYSPACE=k` the counter wraps
//!   at `k`, so repeated queries hit the cache like real traffic does.
//! - By default the load is closed-loop: `concurrency` workers send back to back.
//!   With `RATE=r` it is open-loop instead: exactly `r` requests per second,
//!   whatever the server does, and latency is measured from each request's
//!   scheduled send time, so a stalling server can't hide queueing delay.
//! - `IDENTITY=1` disables response compression (measures raw server cost).

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn arg(i: usize) -> Option<String> {
    std::env::args().nth(i)
}

fn env<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn target(url: &str) -> String {
    if !url.contains("{n}") {
        return url.to_owned();
    }
    let mut n = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Some(k) = env::<u64>("KEYSPACE") {
        n %= k.max(1);
    }
    url.replace("{n}", &n.to_string())
}

/// Status code (0 = transport error) and latency in µs of one request.
type Sample = (u16, u64);

async fn send(client: &reqwest::Client, url: &str, key: &str, since: Instant) -> Sample {
    match client.get(url).header("x-api-key", key).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let ok = resp.bytes().await.is_ok();
            (if ok { status } else { 0 }, since.elapsed().as_micros() as u64)
        }
        Err(_) => (0, since.elapsed().as_micros() as u64),
    }
}

#[tokio::main]
async fn main() {
    let url = arg(1).unwrap_or_else(|| "http://127.0.0.1:8080/health".into());
    let key = arg(2).unwrap_or_default();
    let concurrency: usize = arg(3).and_then(|s| s.parse().ok()).unwrap_or(256);
    let secs: u64 = arg(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    let rate: Option<f64> = env("RATE");

    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .tcp_nodelay(true);
    if std::env::var_os("IDENTITY").is_some() {
        builder = builder.no_gzip().no_brotli().no_deflate();
    }
    let client = builder.build().expect("client");

    let started = Instant::now();
    let samples: Vec<Sample> = match rate {
        Some(rate) => {
            println!("load: {url}  rate={rate}/s (open loop)  duration={secs}s");
            let total = (rate * secs as f64) as u64;
            let mut tasks = Vec::with_capacity(total as usize);
            for i in 0..total {
                let scheduled = started + Duration::from_secs_f64(i as f64 / rate);
                tokio::time::sleep_until(scheduled.into()).await;
                let (client, url, key) = (client.clone(), target(&url), key.clone());
                tasks.push(tokio::spawn(async move { send(&client, &url, &key, scheduled).await }));
            }
            futures::future::join_all(tasks)
                .await
                .into_iter()
                .map(|r| r.expect("task"))
                .collect()
        }
        None => {
            println!("load: {url}  concurrency={concurrency}  duration={secs}s");
            let deadline = started + Duration::from_secs(secs);
            let workers = (0..concurrency).map(|_| {
                let (client, url, key) = (client.clone(), url.clone(), key.clone());
                tokio::spawn(async move {
                    let mut out = Vec::with_capacity(16 * 1024);
                    while Instant::now() < deadline {
                        out.push(send(&client, &target(&url), &key, Instant::now()).await);
                    }
                    out
                })
            });
            futures::future::join_all(workers)
                .await
                .into_iter()
                .flat_map(|w| w.expect("worker"))
                .collect()
        }
    };
    let elapsed = started.elapsed().as_secs_f64();

    let mut codes: BTreeMap<u16, u64> = BTreeMap::new();
    let mut lat: Vec<u64> = Vec::with_capacity(samples.len());
    for (status, us) in &samples {
        *codes.entry(*status).or_default() += 1;
        lat.push(*us);
    }
    let errors = codes.remove(&0).unwrap_or(0);
    lat.sort_unstable();
    let pct = |p: f64| -> f64 {
        if lat.is_empty() {
            return 0.0;
        }
        lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)] as f64 / 1000.0
    };
    println!("requests: {}  errors: {errors}  status: {codes:?}", samples.len());
    println!("throughput: {:.0} req/s", samples.len() as f64 / elapsed);
    println!(
        "latency ms: p50={:.2} p90={:.2} p99={:.2} p99.9={:.2} max={:.2}",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(0.999),
        pct(1.0)
    );
}
