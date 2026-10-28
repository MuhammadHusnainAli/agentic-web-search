//! Engine parsers against captured / modelled SERP fixtures.

use agentic_web_search::{
    Config,
    engines::{Engines, PageQuery},
    model::{EngineKind, RawResult, Region, SafeSearch, TimeRange},
};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn parse(kind: EngineKind, file: &str) -> Vec<RawResult> {
    let engines = Engines::new(&Config::default());
    let html = fixture(file);
    let engine = engines.get(kind);
    assert!(!engine.is_blocked(200, &html), "{file} wrongly detected as blocked");
    engine.parse(&html)
}

fn blocked(kind: EngineKind, status: u16, file: &str) -> bool {
    Engines::new(&Config::default())
        .get(kind)
        .is_blocked(status, &fixture(file))
}

fn assert_well_formed(results: &[RawResult], min: usize) {
    assert!(
        results.len() >= min,
        "expected >= {min} results, got {}: {results:#?}",
        results.len()
    );
    for r in results {
        assert!(!r.title.trim().is_empty(), "empty title: {r:?}");
        assert!(
            r.url.starts_with("http://") || r.url.starts_with("https://"),
            "bad url: {r:?}"
        );
        assert!(!r.title.contains('<'), "markup leaked into title: {r:?}");
    }
}

#[test]
fn bing_parses_real_serp() {
    let r = parse(EngineKind::Bing, "bing.html");
    assert_well_formed(&r, 8);
    // Redirect wrappers (bing.com/ck/a) must be unwrapped to the target site.
    assert!(r.iter().all(|x| !x.url.contains("bing.com/ck/")), "{r:#?}");
    assert!(r.iter().any(|x| x.url.contains("rust-lang.org")));
    assert!(r.iter().filter(|x| !x.snippet.is_empty()).count() >= r.len() / 2);
}

#[test]
fn brave_parses_real_serp() {
    let r = parse(EngineKind::Brave, "brave.html");
    assert_well_formed(&r, 10);
    assert_eq!(r[0].url, "https://rust-lang.org/en-US/");
    assert_eq!(r[0].title, "Rust Programming Language");
    assert!(r.iter().filter(|x| !x.snippet.is_empty()).count() >= r.len() / 2);
}

#[test]
fn duckduckgo_parses_real_serp() {
    let r = parse(EngineKind::DuckDuckGo, "duckduckgo.html");
    assert_well_formed(&r, 10);
    assert!(r.iter().all(|x| !x.url.contains("duckduckgo.com")), "{r:#?}");
    assert!(r.iter().all(|x| !x.snippet.is_empty()));
}

#[test]
fn yahoo_parses_real_serp() {
    let r = parse(EngineKind::Yahoo, "yahoo.html");
    assert_well_formed(&r, 6);
    assert_eq!(r[0].url, "https://rust-lang.org/");
    assert!(r[0].title.contains("Rust Programming Language"));
    assert!(!r[0].snippet.is_empty());
}

#[test]
fn google_parses_modelled_serp() {
    let r = parse(EngineKind::Google, "google.html");
    assert_well_formed(&r, 3);
    assert_eq!(r.len(), 3, "duplicates / internal links must be dropped: {r:#?}");
    assert_eq!(r[0].url, "https://www.rust-lang.org/");
    assert_eq!(r[1].url, "https://doc.rust-lang.org/book/");
    assert!(r[1].snippet.contains("Klabnik"));
}

#[test]
fn mojeek_parses_modelled_serp() {
    let r = parse(EngineKind::Mojeek, "mojeek.html");
    assert_well_formed(&r, 3);
    assert_eq!(r[2].title, "GitHub - rust-lang/rust");
}

#[test]
fn detects_bot_walls() {
    assert!(blocked(EngineKind::DuckDuckGo, 202, "duckduckgo_captcha.html"));
    assert!(blocked(EngineKind::Mojeek, 200, "mojeek_challenge.html"));
    assert!(blocked(EngineKind::Google, 200, "google_enablejs.html"));
    assert!(blocked(EngineKind::Bing, 429, "bing.html"));
}

#[test]
fn every_engine_builds_paginated_requests() {
    let engines = Engines::new(&Config::default());
    for kind in EngineKind::ALL {
        let e = engines.get(kind);
        let q = |page| PageQuery {
            query: "rust & tokio",
            page,
            region: Region::DEFAULT,
            safe: SafeSearch::Moderate,
            time_range: None,
        };
        let (first, second) = (e.http_request(&q(0)), e.http_request(&q(1)));
        assert_ne!(
            (first.url.as_str(), &first.form),
            (second.url.as_str(), &second.form),
            "{kind}: page 2 must differ from page 1"
        );
        let encoded = first.url.as_str().contains("rust+%26+tokio")
            || first
                .form
                .as_ref()
                .is_some_and(|f| f.contains(&("q", "rust & tokio".into())));
        assert!(encoded, "{kind}: query not passed: {first:?}");
        let page1 = e.browser_url(&q(0)).expect("page 1 is always browser-renderable");
        assert!(page1.as_str().contains("rust+%26+tokio"), "{kind}: {page1}");
        assert!(!e.ready_selector().is_empty());
    }
}

#[test]
fn time_range_maps_to_each_engines_native_filter() {
    let engines = Engines::new(&Config::default());
    let expect = [
        (EngineKind::Google, TimeRange::Week, "tbs=qdr%3Aw"),
        (EngineKind::Bing, TimeRange::Day, "filters=ex1%3A%22ez1%22"),
        (EngineKind::Bing, TimeRange::Year, "ez5_"),
        (EngineKind::Brave, TimeRange::Month, "tf=pm"),
        (EngineKind::Yahoo, TimeRange::Day, "btf=d"),
    ];
    for (kind, range, needle) in expect {
        let q = PageQuery {
            query: "rust",
            page: 0,
            region: Region::DEFAULT,
            safe: SafeSearch::Moderate,
            time_range: Some(range),
        };
        let url = engines.get(kind).http_request(&q).url;
        assert!(url.as_str().contains(needle), "{kind}/{range:?}: {url}");
    }
    let q = PageQuery {
        query: "rust",
        page: 0,
        region: Region::DEFAULT,
        safe: SafeSearch::Moderate,
        time_range: Some(TimeRange::Year),
    };
    let ddg = engines.get(EngineKind::DuckDuckGo).http_request(&q);
    assert!(ddg.form.unwrap().contains(&("df", "y".into())));
    // Unsupported ranges are left out rather than sent as garbage.
    assert!(
        !engines
            .get(EngineKind::Yahoo)
            .http_request(&q)
            .url
            .as_str()
            .contains("btf")
    );
    assert!(!engines.get(EngineKind::Mojeek).supports_time_range(TimeRange::Day));
}
