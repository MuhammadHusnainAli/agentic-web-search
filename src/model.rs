//! Core domain types shared by engines, the search service and the API layer.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// Supported search engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[serde(alias = "ddg")]
    DuckDuckGo,
    Google,
    Bing,
    Brave,
    Yahoo,
    Mojeek,
}

impl EngineKind {
    pub const ALL: [EngineKind; 6] = [
        EngineKind::DuckDuckGo,
        EngineKind::Google,
        EngineKind::Bing,
        EngineKind::Brave,
        EngineKind::Yahoo,
        EngineKind::Mojeek,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            EngineKind::DuckDuckGo => "duckduckgo",
            EngineKind::Google => "google",
            EngineKind::Bing => "bing",
            EngineKind::Brave => "brave",
            EngineKind::Yahoo => "yahoo",
            EngineKind::Mojeek => "mojeek",
        }
    }

    /// Dense index, used for lock-free per-engine arrays (metrics, circuit breakers).
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for EngineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EngineKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => Ok(EngineKind::DuckDuckGo),
            "google" => Ok(EngineKind::Google),
            "bing" => Ok(EngineKind::Bing),
            "brave" => Ok(EngineKind::Brave),
            "yahoo" => Ok(EngineKind::Yahoo),
            "mojeek" => Ok(EngineKind::Mojeek),
            other => Err(format!("unknown engine `{other}`")),
        }
    }
}

/// Engine selection in a request: a concrete engine or `auto` (fallback chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EngineChoice {
    #[default]
    Auto,
    Engine(EngineKind),
}

impl FromStr for EngineChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("auto") {
            Ok(EngineChoice::Auto)
        } else {
            s.parse().map(EngineChoice::Engine)
        }
    }
}

impl<'de> Deserialize<'de> for EngineChoice {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// How the SERP is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchMode {
    /// HTTP first; fall back to the headless browser when blocked or empty.
    #[default]
    Auto,
    /// Plain HTTP only (cheapest, fastest).
    Http,
    /// Always render with headless Chromium.
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SafeSearch {
    Off,
    #[default]
    Moderate,
    Strict,
}

/// A `<country>-<language>` region such as `us-en`, `de-de`, `gb-en`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Region {
    country: [u8; 2],
    language: [u8; 2],
}

impl Region {
    pub const DEFAULT: Region = Region {
        country: *b"us",
        language: *b"en",
    };

    pub fn country(&self) -> &str {
        // Constructed only from validated ASCII letters.
        std::str::from_utf8(&self.country).unwrap_or("us")
    }

    pub fn language(&self) -> &str {
        std::str::from_utf8(&self.language).unwrap_or("en")
    }
}

impl Default for Region {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl FromStr for Region {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        let (c, l) = s
            .split_once(['-', '_'])
            .ok_or_else(|| format!("invalid region `{s}`, expected <country>-<language> e.g. us-en"))?;
        let two = |p: &str| -> Option<[u8; 2]> {
            let b = p.as_bytes();
            (b.len() == 2 && b.iter().all(u8::is_ascii_lowercase)).then(|| [b[0], b[1]])
        };
        match (two(c), two(l)) {
            (Some(country), Some(language)) => Ok(Region { country, language }),
            _ => Err(format!(
                "invalid region `{s}`, expected <country>-<language> e.g. us-en"
            )),
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.country(), self.language())
    }
}

impl Serialize for Region {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A result as scraped from one SERP page, before normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A normalised, ranked search result returned to callers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    /// 1-based rank across all fetched pages after de-duplication.
    pub position: u32,
    /// 1-based SERP page this result came from.
    pub page: u32,
    pub engine: EngineKind,
    pub title: String,
    pub url: String,
    pub domain: String,
    pub snippet: String,
    /// Extracted readable text of the target page (only with `include_content`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_engines_and_aliases() {
        assert_eq!("DDG".parse::<EngineKind>(), Ok(EngineKind::DuckDuckGo));
        assert_eq!(
            "bing".parse::<EngineChoice>(),
            Ok(EngineChoice::Engine(EngineKind::Bing))
        );
        assert_eq!("AUTO".parse::<EngineChoice>(), Ok(EngineChoice::Auto));
        assert!("altavista".parse::<EngineKind>().is_err());
    }

    #[test]
    fn engine_indices_are_dense() {
        for (i, e) in EngineKind::ALL.iter().enumerate() {
            assert_eq!(e.index(), i);
        }
    }

    #[test]
    fn parses_regions() {
        let r: Region = "DE-de".parse().unwrap();
        assert_eq!((r.country(), r.language()), ("de", "de"));
        assert_eq!("gb_en".parse::<Region>().unwrap().to_string(), "gb-en");
        assert!("usa-en".parse::<Region>().is_err());
        assert!("us".parse::<Region>().is_err());
        assert!("u1-en".parse::<Region>().is_err());
    }
}
