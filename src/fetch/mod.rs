//! I/O layer: plain HTTP fetching and headless-browser rendering.

pub mod browser;
pub mod http;

pub use browser::{BrowserPool, BrowserStatus};
pub use http::{Fetched, HttpFetcher};
