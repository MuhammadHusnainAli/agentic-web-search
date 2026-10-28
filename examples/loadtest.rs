//! Minimal closed-loop HTTP load generator.
//!
//! ```sh
//! cargo run --release --example loadtest -- \
//!   http://127.0.0.1:8080/v1/search?q=rust&engine=brave  <api-key>  <concurrency=256>  <seconds=10>
//! ```
//!
//! Reports throughput, status codes and latency percentiles.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

fn arg(i: usize) -> Option<String> {
    std::env::args().nth(i)
}

#[tokio::main]
async fn main() {
    let url = arg(1).unwrap_or_else(|| "http://127.0.0.1:8080/health".into());
    let key = arg(2).unwrap_or_default();
    let concurrency: usize = arg(3).and_then(|s| s.parse().ok()).unwrap_or(256);
    let secs: u64 = arg(4).and_then(|s| s.parse().ok()).unwrap_or(10);

    // Set IDENTITY=1 to disable response compression (measures raw server cost).
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .tcp_nodelay(true);
    if std::env::var_os("IDENTITY").is_some() {
        builder = builder.no_gzip().no_brotli().no_deflate();
    }
    let client = builder.build().expect("client");
    let deadline = Instant::now() + Duration::from_secs(secs);
    println!("load: {url}  concurrency={concurrency}  duration={secs}s");

    let workers = (0..concurrency).map(|_| {
        let client = client.clone();
        let url = url.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let mut lat = Vec::with_capacity(16 * 1024);
            let mut codes: BTreeMap<u16, u64> = BTreeMap::new();
            let mut errors = 0u64;
            while Instant::now() < deadline {
                let t = Instant::now();
                match client.get(&url).header("x-api-key", &key).send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let _ = resp.bytes().await;
                        *codes.entry(status).or_default() += 1;
                        lat.push(t.elapsed().as_micros() as u64);
                    }
                    Err(_) => errors += 1,
                }
            }
            (lat, codes, errors)
        })
    });

    let started = Instant::now();
    let mut all = Vec::new();
    let mut codes: BTreeMap<u16, u64> = BTreeMap::new();
    let mut errors = 0;
    for w in futures::future::join_all(workers).await {
        let (lat, c, e) = w.expect("worker");
        all.extend(lat);
        for (k, v) in c {
            *codes.entry(k).or_default() += v;
        }
        errors += e;
    }
    let elapsed = started.elapsed().as_secs_f64();
    all.sort_unstable();
    let pct = |p: f64| -> f64 {
        if all.is_empty() {
            return 0.0;
        }
        all[((all.len() as f64 * p) as usize).min(all.len() - 1)] as f64 / 1000.0
    };
    println!("requests: {}  errors: {errors}  status: {codes:?}", all.len());
    println!("throughput: {:.0} req/s", all.len() as f64 / elapsed);
    println!(
        "latency ms: p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(1.0)
    );
}
