//! Query suggestions and instant answers, from the engines' own keyless JSON
//! endpoints (the ones their search boxes call).

use std::collections::HashSet;

use serde_json::Value;
use url::Url;

use crate::model::{Answer, AnswerKind, EngineKind, Region, SafeSearch};

/// Engine whose suggestion endpoint serves `engine`: Yahoo and Mojeek have
/// none, so DuckDuckGo stands in.
pub fn suggest_engine(engine: EngineKind) -> EngineKind {
    match engine {
        EngineKind::Google | EngineKind::Bing | EngineKind::Brave => engine,
        _ => EngineKind::DuckDuckGo,
    }
}

/// `base` overrides the endpoint host (mirrors, tests); paths stay the same.
pub fn suggest_url(engine: EngineKind, query: &str, region: Region, base: Option<&Url>) -> Url {
    let (host, path) = match suggest_engine(engine) {
        EngineKind::Google => ("https://suggestqueries.google.com/", "complete/search"),
        EngineKind::Bing => ("https://api.bing.com/", "osjson.aspx"),
        EngineKind::Brave => ("https://search.brave.com/", "api/suggest"),
        _ => ("https://duckduckgo.com/", "ac/"),
    };
    let root = base.cloned().unwrap_or_else(|| Url::parse(host).expect("static url"));
    let mut url = root.join(path).expect("static path");
    {
        let mut p = url.query_pairs_mut();
        match suggest_engine(engine) {
            EngineKind::Google => {
                p.append_pair("client", "firefox")
                    .append_pair("hl", region.language())
                    .append_pair("gl", region.country())
                    .append_pair("q", query);
            }
            EngineKind::Bing => {
                let market = format!("{}-{}", region.language(), region.country().to_ascii_uppercase());
                p.append_pair("query", query).append_pair("market", &market);
            }
            EngineKind::Brave => {
                p.append_pair("q", query).append_pair("rich", "false");
            }
            _ => {
                p.append_pair("q", query)
                    .append_pair("type", "list")
                    .append_pair("kl", &region.to_string());
            }
        }
    }
    url
}

/// Every endpoint above answers in the OpenSearch suggestions format,
/// `[query, [suggestion, …], …]`. The query itself and duplicates are dropped.
pub fn parse_suggestions(body: &str, query: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let query = norm(query);
    let mut seen = HashSet::new();
    v.get(1)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && norm(s) != query && seen.insert(norm(s)))
        .take(10)
        .map(str::to_owned)
        .collect()
}

/// DuckDuckGo's Instant Answer endpoint.
pub fn answer_url(query: &str, region: Region, safe: SafeSearch, base: Option<&Url>) -> Url {
    let mut url = base
        .cloned()
        .unwrap_or_else(|| Url::parse("https://api.duckduckgo.com/").expect("static url"));
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json")
        .append_pair("no_html", "1")
        .append_pair("no_redirect", "1")
        .append_pair("skip_disambig", "1")
        .append_pair("kl", &region.to_string())
        .append_pair(
            "kp",
            match safe {
                SafeSearch::Off => "-2",
                SafeSearch::Moderate => "-1",
                SafeSearch::Strict => "1",
            },
        );
    url
}

/// The most direct thing the Instant Answer has: a computed answer, else a
/// (usually Wikipedia) summary, else a dictionary definition.
pub fn parse_answer(body: &str) -> Option<Answer> {
    let v: Value = serde_json::from_str(body).ok()?;
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let heading = field("Heading");
    let (kind, text, source, url) = if let Some(text) = field("Answer") {
        (AnswerKind::Answer, text, Some("DuckDuckGo".to_owned()), None)
    } else if let Some(text) = field("AbstractText") {
        (
            AnswerKind::Abstract,
            text,
            field("AbstractSource"),
            field("AbstractURL"),
        )
    } else {
        (
            AnswerKind::Definition,
            field("Definition")?,
            field("DefinitionSource"),
            field("DefinitionURL"),
        )
    };
    Some(Answer {
        kind,
        text,
        heading,
        source,
        url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_suggest_urls_per_engine() {
        let r: Region = "de-de".parse().unwrap();
        let u = suggest_url(EngineKind::Bing, "rust async", r, None);
        assert_eq!(
            u.as_str(),
            "https://api.bing.com/osjson.aspx?query=rust+async&market=de-DE"
        );
        let u = suggest_url(EngineKind::Google, "x", r, None);
        assert!(
            u.as_str()
                .starts_with("https://suggestqueries.google.com/complete/search?client=firefox&hl=de")
        );
        let u = suggest_url(EngineKind::Mojeek, "x", r, None);
        assert_eq!(u.as_str(), "https://duckduckgo.com/ac/?q=x&type=list&kl=de-de");
        let base = Url::parse("http://127.0.0.1:9/").unwrap();
        let u = suggest_url(EngineKind::Brave, "x", r, Some(&base));
        assert_eq!(u.as_str(), "http://127.0.0.1:9/api/suggest?q=x&rich=false");
    }

    #[test]
    fn parses_opensearch_suggestions() {
        // Real responses (Google adds a third element; Bing longer lists).
        let body = r#"["rust async",["rust async","rust async trait","Rust Async Trait","rust async book"],[],{"google:suggestsubtypes":[[512]]}]"#;
        assert_eq!(
            parse_suggestions(body, "Rust  async "),
            ["rust async trait", "rust async book"]
        );
        assert!(parse_suggestions("<html>blocked</html>", "x").is_empty());
        assert!(parse_suggestions(r#"["x"]"#, "x").is_empty());
    }

    #[test]
    fn parses_instant_answers() {
        let abs = r#"{"Heading":"Eiffel Tower","AbstractText":"The Eiffel Tower is a lattice tower in Paris.",
            "AbstractSource":"Wikipedia","AbstractURL":"https://en.wikipedia.org/wiki/Eiffel_Tower","Answer":"","Definition":""}"#;
        let a = parse_answer(abs).unwrap();
        assert_eq!(a.kind, AnswerKind::Abstract);
        assert_eq!(a.heading.as_deref(), Some("Eiffel Tower"));
        assert_eq!(a.source.as_deref(), Some("Wikipedia"));
        assert_eq!(a.url.as_deref(), Some("https://en.wikipedia.org/wiki/Eiffel_Tower"));

        let calc = r#"{"Heading":"","AbstractText":"","Answer":"42","AnswerType":"calc"}"#;
        assert_eq!(parse_answer(calc).unwrap().kind, AnswerKind::Answer);
        // `Answer` is sometimes an object; that's not a text answer.
        let obj = r#"{"Answer":{"x":1},"AbstractText":"","Definition":"a word","DefinitionSource":"Wordnik"}"#;
        assert_eq!(parse_answer(obj).unwrap().kind, AnswerKind::Definition);
        assert!(parse_answer(r#"{"Heading":"","AbstractText":"","Answer":""}"#).is_none());
        assert!(parse_answer("not json").is_none());
    }
}
