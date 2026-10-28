//! Fake search engines for load testing the full (cache-miss) pipeline
//! without hammering real engines.
//!
//! Serves SERPs in each engine's real markup, plus result pages for content
//! extraction. Point the service at it with `ENGINE_<NAME>_URL`.
//!
//! ```sh
//! MOCK_DELAY_MS=50 MOCK_SLOW=bing MOCK_SLOW_MS=2000 \
//!   cargo run --release --example mock_upstream -- 127.0.0.1:9100
//! ```

use std::{collections::HashMap, fmt::Write, time::Duration};

use axum::{
    Router,
    extract::{Path, Query},
    response::Html,
    routing::{get, post},
};

fn env_ms(key: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// Simulated engine latency; `MOCK_SLOW` names one engine that takes `MOCK_SLOW_MS`.
async fn latency(engine: &str) {
    let slow = std::env::var("MOCK_SLOW").is_ok_and(|s| s == engine);
    let d = if slow {
        env_ms("MOCK_SLOW_MS", 2000)
    } else {
        env_ms("MOCK_DELAY_MS", 50)
    };
    tokio::time::sleep(d).await;
}

fn base() -> String {
    std::env::var("MOCK_BASE").unwrap_or_else(|_| "http://127.0.0.1:9100".into())
}

/// Ten results per page. Engines rotate the same pool of URLs so merged
/// searches see realistic overlap.
fn results(engine_idx: usize, q: &str, page: u32) -> Vec<(String, String, String)> {
    let slug: String = q.chars().filter(char::is_ascii_alphanumeric).take(40).collect();
    (0..10)
        .map(|i| {
            let n = (i + engine_idx * 3) % 12 + page as usize * 12;
            (
                format!("{}/doc/{slug}/{n}", base()),
                format!("{q} result {n}"),
                format!("All about {q}: detailed snippet number {n} with useful context."),
            )
        })
        .collect()
}

fn page_of(params: &HashMap<String, String>, key: &str, per_page: u32, offset: u32) -> u32 {
    params
        .get(key)
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(0, |v| v.saturating_sub(offset) / per_page)
}

async fn search(Query(p): Query<HashMap<String, String>>) -> Html<String> {
    let mut out = String::from("<html><head><title>results</title></head><body>");
    if p.contains_key("setlang") {
        latency("bing").await;
        let q = &p["q"];
        out.push_str(r#"<ol id="b_results">"#);
        for (u, t, s) in results(0, q, page_of(&p, "first", 10, 1)) {
            let _ = write!(
                out,
                r#"<li class="b_algo"><h2><a href="{u}">{t}</a></h2><div class="b_caption"><p>{s}</p></div></li>"#
            );
        }
        out.push_str("</ol>");
    } else if p.contains_key("source") {
        latency("brave").await;
        let q = &p["q"];
        for (u, t, s) in results(1, q, page_of(&p, "offset", 1, 0)) {
            let _ = write!(
                out,
                r#"<div class="snippet" data-type="web"><a href="{u}"><div class="title">{t}</div></a><div class="snippet-description">{s}</div></div>"#
            );
        }
    } else if p.contains_key("lb") {
        latency("mojeek").await;
        let q = &p["q"];
        out.push_str(r#"<ul class="results-standard">"#);
        for (u, t, s) in results(2, q, page_of(&p, "s", 10, 1)) {
            let _ = write!(
                out,
                r#"<li><h2><a class="title" href="{u}">{t}</a></h2><p class="s">{s}</p></li>"#
            );
        }
        out.push_str("</ul>");
    } else if p.contains_key("p") {
        latency("yahoo").await;
        let q = &p["p"];
        for (u, t, s) in results(3, q, page_of(&p, "b", 7, 1)) {
            let _ = write!(
                out,
                r#"<div class="algo-sr"><div class="compTitle"><h3><a href="{u}">{t}</a></h3></div><div class="compText"><p>{s}</p></div></div>"#
            );
        }
    } else if p.contains_key("hl") {
        latency("google").await;
        let q = &p["q"];
        out.push_str(r#"<div id="search">"#);
        for (u, t, s) in results(4, q, page_of(&p, "start", 10, 0)) {
            let _ = write!(
                out,
                r#"<div class="g"><a href="{u}"><h3>{t}</h3></a><div class="VwiC3b">{s}</div></div>"#
            );
        }
        out.push_str("</div>");
    }
    out.push_str("</body></html>");
    Html(out)
}

async fn duckduckgo(body: String) -> Html<String> {
    latency("duckduckgo").await;
    let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
    let q = form.get("q").cloned().unwrap_or_default();
    let page = form.get("s").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) / 10;
    let mut out = String::from("<html><head><title>ddg</title></head><body>");
    for (u, t, s) in results(5, &q, page) {
        let _ = write!(
            out,
            r#"<div class="result"><a class="result__a" href="{u}">{t}</a><a class="result__snippet">{s}</a></div>"#
        );
    }
    out.push_str("</body></html>");
    Html(out)
}

/// A ~25 KB article with the chrome real pages have (nav, scripts, footer).
async fn doc(Path((slug, n)): Path<(String, u32)>) -> Html<String> {
    tokio::time::sleep(env_ms("MOCK_DOC_DELAY_MS", 30)).await;
    let mut out = format!(
        "<html><head><title>{slug} {n}</title><script>var analytics = {{}};</script><style>body{{}}</style></head>\
         <body><nav><a href='/'>Home</a><a href='/about'>About</a></nav><article><h1>{slug} document {n}</h1>"
    );
    for p in 0..60 {
        let _ = write!(
            out,
            "<p>Paragraph {p} of document {n} about {slug}. It explains the topic in reasonable depth \
             so that extraction has real work to do, with <a href='#'>links</a> and <b>markup</b>.</p>"
        );
    }
    out.push_str("</article><footer>Copyright</footer></body></html>");
    Html(out)
}

#[tokio::main]
async fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:9100".into());
    let app = Router::new()
        .route("/search", get(search))
        .route("/html/", post(duckduckgo))
        .route("/doc/{slug}/{n}", get(doc));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("mock upstream on http://{addr}");
    axum::serve(listener, app).await.expect("serve");
}
