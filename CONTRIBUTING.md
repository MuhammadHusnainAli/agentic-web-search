# Contributing

Thanks for helping! The bar is simple: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` must pass, for both the default build and `--no-default-features`.

## Layout

| Path | What lives there |
|---|---|
| `src/app/main.rs` | Binary entrypoint: config, runtime, graceful shutdown, `healthcheck` subcommand |
| `src/api/` | Router, middleware stack, auth + rate limiting, request/response models |
| `src/search/` | Orchestration: cache + coalescing, page fan-out, `auto` fallback, relevance guard, content extraction |
| `src/engines/` | One file per search engine. Pure & synchronous: build request, detect blocks, parse HTML |
| `src/fetch/` | I/O: pooled HTTP client (SSRF-guarded for content) and the headless Chromium pool |
| `tests/parsers.rs` | Parser tests against `tests/fixtures/*.html` |
| `tests/api.rs` | End-to-end API tests against a wiremock upstream |

## Adding a search engine

1. Add a variant to `EngineKind` in `src/model.rs` (keep `ALL` and `as_str` in sync).
2. Create `src/engines/<name>.rs` implementing `Engine`:
   - `http_request` – URL/form for page *n* (0-based)
   - `ready_selector` – CSS selector that means "results rendered" (browser mode)
   - `is_blocked` – start with `generic_block`, add engine-specific captcha markers
   - `parse` – compile selectors once in a `LazyLock`, skip ads, unwrap redirect links
3. Register it in `Engines::new` (order must match the enum discriminants).
4. Save a real SERP as `tests/fixtures/<name>.html` (strip `<script>`/`<style>`) and add a test in `tests/parsers.rs`.

## When an engine changes its markup

Engines change HTML regularly. Capture a fresh page, replace the fixture, run
`cargo test --test parsers`, and adjust selectors until it passes.

## Commit style

Small, focused commits with an imperative subject line ("Add Qwant engine").
