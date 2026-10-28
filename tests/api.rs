//! End-to-end API tests: the real router, with every search engine pointed at a
//! local mock server.

use std::time::Duration;

use agentic_web_search::{AppState, Config, api, model::EngineKind};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path, query_param, query_param_is_missing},
};

const KEY: &str = "test-key-0123456789";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn config(mock: &MockServer) -> Config {
    let mut cfg = Config {
        api_keys: vec![KEY.into()],
        rate_limit_per_sec: 1_000,
        rate_limit_burst: 1_000,
        content_allow_private_ips: true,
        upstream_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    cfg.browser.enabled = false;
    for kind in EngineKind::ALL {
        cfg.engine_base_urls.insert(kind, format!("{}/", mock.uri()));
    }
    cfg
}

fn app(cfg: Config) -> Router {
    api::router(AppState::new(cfg).expect("state"))
}

/// Mojeek-style SERP whose links point at `base` (so content fetches hit the mock).
/// Snippets mention `query` so the result set passes the relevance guard.
fn mojeek_page(base: &str, query: &str, page: u32, n: u32) -> String {
    let items: String = (1..=n)
        .map(|i| {
            format!(
                r#"<li><h2><a class="title" href="{base}/doc/{page}/{i}">Result {page}-{i}</a></h2><p class="s">About {query}, snippet {page}-{i}</p></li>"#
            )
        })
        .collect();
    format!(
        r#"<html><head><title>q - Mojeek</title></head><body><ul class="results-standard">{items}</ul></body></html>"#
    )
}

fn html(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into(), "text/html; charset=utf-8")
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    json: Value,
}

async fn call(app: &Router, req: Request<Body>) -> Resp {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()));
    Resp { status, headers, json }
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).header("x-api-key", KEY).body(Body::empty()).unwrap()
}

fn post(body: Value) -> Request<Body> {
    Request::post("/v1/search")
        .header("x-api-key", KEY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn health_is_public() {
    let mock = MockServer::start().await;
    let app = app(config(&mock));
    let r = call(&app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["status"], "ok");
    let r = call(&app, Request::get("/ready").body(Body::empty()).unwrap()).await;
    assert_eq!(r.json["browser"], "disabled");
    assert!(r.headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn rejects_missing_and_wrong_keys() {
    let mock = MockServer::start().await;
    let app = app(config(&mock));
    let r = call(&app, Request::get("/v1/search?q=rust").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json["error"]["code"], "unauthorized");

    let req = Request::get("/v1/search?q=rust")
        .header("x-api-key", "wrong-key-0123456789")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&app, req).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bing_search_returns_structured_results_and_caches() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust programming"))
        .and(query_param("mkt", "en-US"))
        .respond_with(html(fixture("bing.html")))
        .expect(1) // second call must be served from cache
        .mount(&mock)
        .await;
    let app = app(config(&mock));

    let req = Request::get("/v1/search?q=rust%20programming&engine=bing")
        .header(header::AUTHORIZATION, format!("Bearer {KEY}"))
        .body(Body::empty())
        .unwrap();
    let r = call(&app, req).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engine"], "bing");
    assert_eq!(r.json["cached"], false);
    assert_eq!(r.headers["x-cache"], "MISS");
    let results = r.json["results"].as_array().unwrap();
    assert!(results.len() >= 8);
    for (i, res) in results.iter().enumerate() {
        assert_eq!(res["position"], i + 1);
        assert_eq!(res["page"], 1);
        assert_eq!(res["engine"], "bing");
        assert!(res["url"].as_str().unwrap().starts_with("http"));
        assert!(!res["domain"].as_str().unwrap().is_empty());
        assert!(res.get("content").is_none());
    }
    assert_eq!(r.json["total_results"], results.len());

    let r2 = call(&app, get("/v1/search?q=rust%20%20programming&engine=bing")).await;
    assert_eq!(r2.status, StatusCode::OK);
    assert_eq!(r2.json["cached"], true, "whitespace-normalised query should hit cache");
    assert_eq!(r2.headers["x-cache"], "HIT");
    assert_eq!(r2.json["results"], r.json["results"]);
}

#[tokio::test]
async fn concurrent_identical_requests_are_coalesced() {
    let mock = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(html(fixture("brave.html")).set_delay(Duration::from_millis(200)))
        .expect(1)
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let reqs = (0..50).map(|_| call(&app, get("/v1/search?q=rust%20programming&engine=brave")));
    let out = futures::future::join_all(reqs).await;
    assert!(out.iter().all(|r| r.status == StatusCode::OK));
    assert_eq!(out.iter().filter(|r| r.json["cached"] == false).count(), 1);
}

#[tokio::test]
async fn fetches_multiple_pages_concurrently_and_ranks_them() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    Mock::given(path("/search"))
        .and(query_param_is_missing("s"))
        .respond_with(html(mojeek_page(&base, "paging", 1, 4)))
        .mount(&mock)
        .await;
    for (page, s) in [(2, "11"), (3, "21")] {
        Mock::given(path("/search"))
            .and(query_param("s", s))
            .respond_with(html(mojeek_page(&base, "paging", page, 4)))
            .mount(&mock)
            .await;
    }
    let app = app(config(&mock));
    let r = call(&app, post(json!({"query": "paging", "engine": "mojeek", "pages": 3}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["pages_requested"], 3);
    assert_eq!(r.json["pages_fetched"], 3);
    let results = r.json["results"].as_array().unwrap();
    assert_eq!(results.len(), 12);
    assert_eq!(results[0]["title"], "Result 1-1");
    assert_eq!(results[4]["title"], "Result 2-1");
    assert_eq!(results[4]["page"], 2);
    assert_eq!(results[11]["position"], 12);
    assert_eq!(results[11]["page"], 3);

    let r = call(
        &app,
        post(json!({"query": "paging", "engine": "mojeek", "pages": 3, "max_results": 5})),
    )
    .await;
    assert_eq!(r.json["results"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn partial_page_failures_are_reported_not_fatal() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    Mock::given(path("/search"))
        .and(query_param_is_missing("s"))
        .respond_with(html(mojeek_page(&base, "partial", 1, 3)))
        .mount(&mock)
        .await;
    Mock::given(path("/search"))
        .and(query_param("s", "11"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/search?q=partial&engine=mojeek&pages=2")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["pages_fetched"], 1);
    assert_eq!(r.json["results"].as_array().unwrap().len(), 3);
    assert_eq!(r.json["errors"][0]["engine"], "mojeek");
    assert_eq!(r.json["errors"][0]["page"], 2);
}

#[tokio::test]
async fn auto_falls_back_past_blocked_engine() {
    let mock = MockServer::start().await;
    // Bing (first in the chain) answers with a captcha wall; Brave works.
    Mock::given(path("/search"))
        .and(query_param("mkt", "en-US"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html><head><title>Captcha</title></head></html>", "text/html"),
        )
        .mount(&mock)
        .await;
    Mock::given(path("/search"))
        .and(query_param("source", "web"))
        .respond_with(html(fixture("brave.html")))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/search?q=rust%20programming")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engine"], "brave");
    assert_eq!(r.json["engines_tried"], json!(["bing", "brave"]));
    assert_eq!(r.json["errors"][0]["engine"], "bing");
    assert_eq!(r.json["errors"][0]["blocked"], true);
}

#[tokio::test]
async fn irrelevant_serps_are_treated_as_soft_blocks() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    // Bing and Brave answer with (real) SERPs about something else entirely.
    Mock::given(path("/search"))
        .and(query_param("mkt", "en-US"))
        .respond_with(html(fixture("bing.html")))
        .mount(&mock)
        .await;
    Mock::given(path("/search"))
        .and(query_param("source", "web"))
        .respond_with(html(fixture("brave.html")))
        .mount(&mock)
        .await;
    Mock::given(path("/search"))
        .and(query_param("lb", "en"))
        .respond_with(html(mojeek_page(&base, "kubernetes operators", 1, 3)))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/search?q=kubernetes%20operators")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engine"], "mojeek");
    assert_eq!(
        r.json["engines_tried"],
        json!(["bing", "brave", "duckduckgo", "yahoo", "mojeek"])
    );
    let errors = r.json["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["engine"] == "bing" && e["blocked"] == true));
    assert!(errors.iter().any(|e| e["engine"] == "brave" && e["blocked"] == true));
}

#[tokio::test]
async fn all_engines_failing_is_a_502_with_details() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_body_raw(fixture("duckduckgo_captcha.html"), "text/html"))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/search?q=doomed")).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.json["error"]["code"], "engine_blocked");
    let details = r.json["error"]["details"].as_array().unwrap();
    // google is skipped (needs browser), the other five were attempted
    assert!(details.iter().any(|d| d["engine"] == "google"));
    assert!(details.iter().filter(|d| d["blocked"] == true).count() >= 5);
}

#[tokio::test]
async fn duckduckgo_uses_form_post() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/html/"))
        .and(body_string_contains("q=rust+programming"))
        .and(body_string_contains("kl=us-en"))
        .respond_with(html(fixture("duckduckgo.html")))
        .expect(1)
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, post(json!({"q": "rust programming", "engine": "ddg"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engine"], "duckduckgo");
    assert_eq!(r.json["results"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn include_content_extracts_page_text() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    Mock::given(path("/search"))
        .respond_with(html(mojeek_page(&base, "content", 1, 3)))
        .mount(&mock)
        .await;
    Mock::given(path("/doc/1/1"))
        .respond_with(html(
            "<html><body><nav>menu</nav><article><h1>Doc one</h1><p>Body text.</p></article></body></html>",
        ))
        .mount(&mock)
        .await;
    Mock::given(path("/doc/1/2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0u8; 16], "application/pdf"))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(
        &app,
        post(json!({"query": "content", "engine": "mojeek", "include_content": true, "content_limit": 2})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let results = r.json["results"].as_array().unwrap();
    assert_eq!(results[0]["content"], "Doc one\nBody text.");
    assert!(results[1].get("content").is_none(), "non-HTML must be skipped");
    assert!(results[2].get("content").is_none(), "beyond content_limit");
}

#[tokio::test]
async fn content_fetch_blocks_private_addresses_by_default() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    Mock::given(path("/search"))
        .respond_with(html(mojeek_page(&base, "ssrf", 1, 1)))
        .mount(&mock)
        .await;
    Mock::given(path("/doc/1/1"))
        .respond_with(html("<body><p>secret internal page</p></body>"))
        .expect(0)
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.content_allow_private_ips = false;
    let app = app(cfg);
    let r = call(
        &app,
        post(json!({"query": "ssrf", "engine": "mojeek", "include_content": true})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json["results"][0].get("content").is_none());
}

#[tokio::test]
async fn validates_input() {
    let mock = MockServer::start().await;
    let app = app(config(&mock));
    for (uri, needle) in [
        ("/v1/search?q=", "empty"),
        ("/v1/search?q=x&pages=0", "pages"),
        ("/v1/search?q=x&pages=99", "pages"),
        ("/v1/search?q=x&engine=altavista", "unknown engine"),
        ("/v1/search?q=x&mode=browser", "browser"),
        ("/v1/search?q=x&region=usa", "region"),
        ("/v1/search?q=x&bogus=1", "bogus"),
        ("/v1/search", "query"),
    ] {
        let r = call(&app, get(uri)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.json);
        assert_eq!(r.json["error"]["code"], "bad_request");
        let msg = r.json["error"]["message"].as_str().unwrap();
        assert!(msg.contains(needle), "{uri}: `{msg}` should mention `{needle}`");
    }
    let bad_json = Request::post("/v1/search")
        .header("x-api-key", KEY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    assert_eq!(call(&app, bad_json).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rate_limits_per_key() {
    let mock = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(html(fixture("brave.html")))
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.rate_limit_per_sec = 1;
    cfg.rate_limit_burst = 2;
    let app = app(cfg);
    let codes: Vec<StatusCode> =
        futures::future::join_all((0..3).map(|_| call(&app, get("/v1/search?q=rust&engine=brave"))))
            .await
            .into_iter()
            .map(|r| r.status)
            .collect();
    assert_eq!(codes.iter().filter(|c| **c == StatusCode::OK).count(), 2, "{codes:?}");
    let r = call(&app, get("/v1/search?q=rust&engine=brave")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json["error"]["code"], "rate_limited");
    assert!(r.headers.contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn engines_metrics_and_404() {
    let mock = MockServer::start().await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/engines")).await;
    let engines = r.json.as_array().unwrap();
    assert_eq!(engines.len(), 6);
    let google = engines.iter().find(|e| e["name"] == "google").unwrap();
    assert_eq!(google["requires_browser"], true);
    assert_eq!(google["available"], false);

    let r = call(&app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    let text = r.json.as_str().unwrap();
    assert!(text.contains("aws_requests_total"));
    assert!(text.contains("aws_upstream_http_total{engine=\"bing\"}"));

    let r = call(&app, get("/nope")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json["error"]["code"], "not_found");
}

#[tokio::test]
async fn auth_can_be_disabled() {
    let mock = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(html(fixture("brave.html")))
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.require_auth = false;
    cfg.api_keys.clear();
    let app = app(cfg);
    let r = call(
        &app,
        Request::get("/v1/search?q=rust&engine=brave")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn compresses_when_client_accepts_it() {
    let mock = MockServer::start().await;
    Mock::given(path("/search"))
        .respond_with(html(fixture("brave.html")))
        .mount(&mock)
        .await;
    let compressed = app(config(&mock));
    let req = Request::get("/v1/search?q=rust&engine=brave")
        .header("x-api-key", KEY)
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let resp = compressed.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");

    let mut cfg = config(&mock);
    cfg.compression = false;
    let req = Request::get("/v1/search?q=rust&engine=brave")
        .header("x-api-key", KEY)
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let resp = app(cfg).oneshot(req).await.unwrap();
    assert!(resp.headers().get(header::CONTENT_ENCODING).is_none());
}

/// Bing-style SERP with arbitrary result URLs.
fn bing_page(urls: &[&str], query: &str) -> String {
    let items: String = urls
        .iter()
        .map(|u| {
            format!(r#"<li class="b_algo"><h2><a href="{u}">{u}</a></h2><div class="b_caption"><p>About {query}: {u}</p></div></li>"#)
        })
        .collect();
    format!(r#"<html><head><title>q - Bing</title></head><body><ol id="b_results">{items}</ol></body></html>"#)
}

fn bing() -> wiremock::MockBuilder {
    Mock::given(path("/search")).and(query_param("mkt", "en-US"))
}

fn mojeek() -> wiremock::MockBuilder {
    Mock::given(path("/search")).and(query_param("lb", "en"))
}

#[tokio::test]
async fn slow_engine_is_hedged_instead_of_waited_for() {
    let mock = MockServer::start().await;
    bing()
        .respond_with(html(bing_page(&["https://slow.example/"], "hedge")).set_delay(Duration::from_secs(4)))
        .mount(&mock)
        .await;
    mojeek()
        .respond_with(html(mojeek_page(&mock.uri(), "hedge", 1, 3)))
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.hedge_delay = Duration::from_millis(150);
    let app = app(cfg);
    let started = std::time::Instant::now();
    let r = call(&app, get("/v1/search?q=hedge&engines=bing,mojeek")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(r.json["engine"], "mojeek");
    assert_eq!(r.json["engines_tried"], json!(["bing", "mojeek"]));
}

#[tokio::test]
async fn race_starts_every_engine_at_once() {
    let mock = MockServer::start().await;
    bing()
        .respond_with(html(bing_page(&["https://a.example/"], "race")).set_delay(Duration::from_millis(800)))
        .mount(&mock)
        .await;
    mojeek()
        .respond_with(html(mojeek_page(&mock.uri(), "race", 1, 2)).set_delay(Duration::from_millis(300)))
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.hedge_delay = Duration::from_secs(10);
    let app = app(cfg);
    let started = std::time::Instant::now();
    let r = call(
        &app,
        post(json!({"query": "race", "engines": ["bing", "mojeek"], "strategy": "race"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engine"], "mojeek");
    assert!(
        started.elapsed() < Duration::from_millis(750),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn merge_fuses_engines_and_drops_stragglers() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    let shared = format!("{base}/doc/1/2");
    bing()
        .respond_with(html(bing_page(&["https://only-bing.example/", &shared], "merge")))
        .mount(&mock)
        .await;
    mojeek()
        .respond_with(html(mojeek_page(&base, "merge", 1, 3)))
        .mount(&mock)
        .await;
    Mock::given(path("/search"))
        .and(query_param("source", "web"))
        .respond_with(html(fixture("brave.html")).set_delay(Duration::from_secs(4)))
        .mount(&mock)
        .await;
    let mut cfg = config(&mock);
    cfg.merge_grace = Duration::from_millis(200);
    let app = app(cfg);
    let r = call(&app, get("/v1/search?q=merge&engines=bing,mojeek,brave&strategy=merge")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["engines_used"], json!(["bing", "mojeek"]));
    let results = r.json["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "shared URL de-duplicated");
    assert_eq!(results[0]["url"], shared, "URL both engines agree on ranks first");
    assert_eq!(results[0]["engines"], json!(["bing", "mojeek"]));
    for (i, res) in results.iter().enumerate() {
        assert_eq!(res["position"], i + 1);
    }
    let errors = r.json["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e["engine"] == "brave" && e["error"].as_str().unwrap().contains("dropped")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn domain_and_file_type_filters() {
    let mock = MockServer::start().await;
    bing()
        .and(query_param(
            "q",
            "tokio (site:a.com OR site:b.org) -site:x.a.com filetype:pdf",
        ))
        .respond_with(html(bing_page(
            &[
                "https://a.com/1",
                "https://www.a.com/2",
                "https://x.a.com/3",
                "https://docs.b.org/4",
                "https://evil.com/5",
                "https://nota.com/6",
            ],
            "tokio",
        )))
        .expect(2) // second request differs only in unique_domains
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(
        &app,
        post(json!({
            "query": "tokio", "engine": "bing", "file_type": "PDF",
            "include_domains": ["b.org", "https://www.a.com/"], "exclude_domains": "x.a.com"
        })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let domains: Vec<&str> = r.json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["domain"].as_str().unwrap())
        .collect();
    assert_eq!(domains, ["a.com", "a.com", "docs.b.org"]);

    let r = call(
        &app,
        post(json!({
            "query": "tokio", "engine": "bing", "file_type": "pdf", "unique_domains": true,
            "include_domains": ["a.com", "b.org"], "exclude_domains": ["x.a.com"]
        })),
    )
    .await;
    assert_eq!(r.json["cached"], false, "unique_domains is part of the cache key");
    let results = r.json["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["position"], 2);
}

#[tokio::test]
async fn time_range_is_forwarded_or_warned_about() {
    let mock = MockServer::start().await;
    bing()
        .and(query_param("filters", r#"ex1:"ez2""#))
        .respond_with(html(bing_page(&["https://a.com/"], "news")))
        .expect(1)
        .mount(&mock)
        .await;
    mojeek()
        .respond_with(html(mojeek_page(&mock.uri(), "news", 1, 2)))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let r = call(&app, get("/v1/search?q=news&engine=bing&time_range=week")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json.get("warnings").is_none());

    let r = call(&app, get("/v1/search?q=news&engine=mojeek&time_range=week")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json["warnings"][0].as_str().unwrap().contains("mojeek"));

    let r = call(&app, get("/v1/engines")).await;
    let mojeek = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "mojeek")
        .unwrap();
    assert_eq!(mojeek["time_ranges"], json!([]));
}

#[tokio::test]
async fn content_respects_deadline_and_char_limit() {
    let mock = MockServer::start().await;
    let base = mock.uri();
    mojeek()
        .respond_with(html(mojeek_page(&base, "deadline", 1, 2)))
        .mount(&mock)
        .await;
    let long = "word ".repeat(400);
    Mock::given(path("/doc/1/1"))
        .respond_with(html(format!("<body><p>{long}</p></body>")))
        .expect(1) // second search reuses the content cache
        .mount(&mock)
        .await;
    Mock::given(path("/doc/1/2"))
        .respond_with(html("<body><p>slow</p></body>").set_delay(Duration::from_secs(3)))
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    let started = std::time::Instant::now();
    let r = call(
        &app,
        post(json!({
            "query": "deadline", "engine": "mojeek", "include_content": true,
            "content_limit": 2, "content_max_chars": 120, "content_timeout_ms": 300
        })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    let results = r.json["results"].as_array().unwrap();
    let content = results[0]["content"].as_str().unwrap();
    assert!(
        content.chars().count() <= 120 && content.ends_with("word"),
        "{content:?}"
    );
    assert!(results[1].get("content").is_none());
    assert!(results[1]["content_error"].as_str().unwrap().contains("timed out"));

    // Different content settings = new search, but page text comes from the content cache.
    let r = call(
        &app,
        post(json!({"query": "deadline", "engine": "mojeek", "include_content": true, "content_limit": 1})),
    )
    .await;
    assert!(r.json["results"][0]["content"].as_str().unwrap().len() > 120);
}

#[tokio::test]
async fn fresh_bypasses_the_cache() {
    let mock = MockServer::start().await;
    mojeek()
        .respond_with(html(mojeek_page(&mock.uri(), "fresh", 1, 2)))
        .expect(2)
        .mount(&mock)
        .await;
    let app = app(config(&mock));
    assert_eq!(
        call(&app, get("/v1/search?q=fresh&engine=mojeek")).await.json["cached"],
        false
    );
    let r = call(&app, get("/v1/search?q=fresh&engine=mojeek&fresh=true")).await;
    assert_eq!(r.json["cached"], false);
    assert_eq!(
        call(&app, get("/v1/search?q=fresh&engine=mojeek")).await.json["cached"],
        true
    );
}

#[tokio::test]
async fn validates_new_params() {
    let mock = MockServer::start().await;
    let app = app(config(&mock));
    for (uri, needle) in [
        ("/v1/search?q=x&engine=bing&engines=brave", "conflicts"),
        ("/v1/search?q=x&engine=bing&strategy=merge", "strategy"),
        ("/v1/search?q=x&strategy=parallel", "strategy"),
        ("/v1/search?q=x&time_range=decade", "time_range"),
        ("/v1/search?q=x&engines=bing,altavista", "unknown engine"),
        ("/v1/search?q=x&include_domains=localhost", "invalid domain"),
        ("/v1/search?q=x&file_type=../etc", "file_type"),
        (
            "/v1/search?q=x&include_content=true&content_timeout_ms=5",
            "content_timeout_ms",
        ),
        (
            "/v1/search?q=x&include_content=true&content_max_chars=999999",
            "content_max_chars",
        ),
    ] {
        let r = call(&app, get(uri)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {}", r.json);
        let msg = r.json["error"]["message"].as_str().unwrap();
        assert!(msg.contains(needle), "{uri}: `{msg}` should mention `{needle}`");
    }
}
