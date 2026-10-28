//! # agentic-web-search
//!
//! Self-hosted web search microservice for AI agents: multi-engine SERP
//! scraping (HTTP fast path, headless Chromium fallback) behind an
//! API-key-protected JSON API.

pub mod api;
pub mod config;
pub mod engines;
pub mod error;
pub mod fetch;
pub mod metrics;
pub mod model;
pub mod search;
pub mod state;
pub mod telemetry;

pub use config::Config;
pub use state::{AppState, SharedState};
