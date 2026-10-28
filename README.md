# agentic-web-search

**Self-hosted web search for AI agents. No third-party search or LLM APIs.**

A single Rust microservice that searches the web on behalf of your agents.
It scrapes Bing, Brave, DuckDuckGo, Yahoo, Mojeek and Google directly.
Plain HTTP is the fast path, and pooled headless Chromium is the fallback.
Results come back as clean, ranked, de-duplicated JSON.

```bash
curl -s localhost:8080/v1/search \
  -H "X-API-Key: $KEY" -H 'content-type: application/json' \
  -d '{"query": "tokio runtime", "engine": "auto", "pages": 2}'
```

```json
{
  "query": "tokio runtime",
  "engine": "brave",
  "engines_tried": ["bing", "brave"],
  "pages_requested": 2,
  "pages_fetched": 2,
  "total_results": 19,
  "cached": false,
  "elapsed_ms": 412,
  "results": [
    {
      "position": 1, "page": 1, "engine": "brave",
      "title": "tokio::runtime - Rust",
      "url": "https://docs.rs/tokio/latest/tokio/runtime/index.html",
      "domain": "docs.rs",
      "snippet": "The Tokio runtime. Unlike other Rust programs, asynchronous applications require runtime support..."
    }
  ],
  "errors": [
    { "engine": "bing", "page": 1, "error": "bing blocked the request (captcha / bot challenge)", "blocked": true }
  ]
}
```

## Features

- **6 engines**: `bing`, `brave`, `duckduckgo` (`ddg`), `yahoo`, `mojeek`, `google`. Also `auto`, which walks a configurable fallback chain until one engine returns results.
- **Pagination**: `pages: 1..=MAX_PAGES`. Pages are fetched **concurrently**, then merged, de-duplicated and re-ranked.
- **HTTP first, browser when needed** (`mode: auto|http|browser`):
  - Captchas, JS walls and *soft blocks* (an engine quietly returning unrelated "junk" results) are detected and retried in headless Chromium.
  - After a block, that engine skips the doomed HTTP attempt for a cooldown period.
- **API keys**: `X-API-Key` or `Authorization: Bearer`. Keys are stored only as SHA-256 digests.
- **Per-key rate limiting** (GCRA), **load shedding** (503 instead of unbounded queues), and request timeouts.
- **Cache with request coalescing**: identical concurrent queries trigger exactly one upstream fetch, and cache hits are served in microseconds.
- **Optional page content**: `include_content: true` fetches the top results and extracts readable text. These fetches are SSRF-guarded: private, loopback and metadata IPs are blocked, including via DNS rebinding and redirects.
- **Observability**: `/health`, `/ready`, Prometheus `/metrics`, `x-request-id`, and JSON logs.
- **Lean**:
  - mimalloc, fat LTO, connection pooling over HTTP/2 and rustls.
  - Response bodies are size-capped.
  - Results are pre-serialised in the cache, so a hit does no JSON re-encoding.
  - One lazily-started Chromium with images disabled and a bounded number of tabs.

## Quick start

```bash
cp .env.example .env              # set API_KEYS
cargo run --release               # http://localhost:8080
```

Docker:

```bash
docker compose up --build                                          # full image with Chromium (~1.1 GB)
docker build --target lite -t agentic-web-search:lite .            # ~53 MB image, ~22 MB RAM idle, HTTP only
docker run -p 8080:8080 -e API_KEYS=$(openssl rand -hex 24) agentic-web-search:lite
```

Build without the browser (smaller binary, no Chromium needed):
`cargo build --release --no-default-features`

## API

### `POST /v1/search` / `GET /v1/search`

| Field | Type | Default | Notes |
|---|---|---|---|
| `query` (`q`) | string | required | ≤ `MAX_QUERY_LEN` chars |
| `engine` | string | `DEFAULT_ENGINE` (`auto`) | `auto`, `bing`, `brave`, `duckduckgo`/`ddg`, `yahoo`, `mojeek`, `google` |
| `engines` | list | `FALLBACK_ENGINES` | engines `auto` may use, in priority order |
| `strategy` | string | `fallback` | `auto` only. `fallback`: in order, hedging slow engines. `race`: all at once, fastest wins. `merge`: all at once, rankings fused |
| `pages` | int | 1 | `1..=MAX_PAGES`, fetched concurrently |
| `mode` | string | `auto` | `auto` (HTTP → browser fallback), `http`, `browser` |
| `region` | string | `us-en` | `<country>-<language>` |
| `safe_search` | string | `moderate` | `off`, `moderate`, `strict` |
| `time_range` | string | – | `day`, `week`, `month`, `year` (native per-engine filter; see `/v1/engines`) |
| `include_domains` (`site`) | list | – | only these domains (subdomains included), ≤ 10 |
| `exclude_domains` | list | – | never these domains, ≤ 10 |
| `file_type` | string | – | e.g. `pdf` |
| `unique_domains` | bool | false | keep only the best result per domain |
| `max_results` | int | – | truncate after merge (≤ 200) |
| `include_content` | bool | false | fetch and extract the readable text of top results |
| `content_limit` | int | 3 | how many results get `content` (≤ `CONTENT_MAX_RESULTS`) |
| `content_format` | string | `markdown` | `markdown` (headings, lists, code, tables, links) or `text` |
| `content_mode` | string | `full` | `full` (page from the top) or `relevant` (only the passages matching the query, with their headings) |
| `content_max_chars` | int | `CONTENT_MAX_CHARS` | per-result text cap, cut on a word boundary |
| `content_timeout_ms` | int | `CONTENT_TIMEOUT_SECS` | content deadline; slower pages get `content_error` instead |
| `include_suggestions` | bool | false | query suggestions from the engine's own suggestion endpoint |
| `include_answer` | bool | false | a direct answer: computed answer, summary (usually Wikipedia) or definition |
| `fresh` | bool | false | bypass the cache (the fresh result is cached) |

GET uses the same fields as query parameters, with lists comma-separated:
`/v1/search?q=rust&engines=bing,brave&strategy=merge&time_range=month&site=docs.rs`.

Each result has `position`, `page`, `engine`, `title`, `url`, `domain`, `snippet` and `score`.
`score` is a rank-fusion relevance in (0, 1]. 1.0 means ranked first by every engine that returned results.
With `strategy=merge` over two engines, a URL only one of them found scores at most 0.5, so
`score > 0.5` keeps the results the engines agree on. It can also have
`engines` (merge only: every engine that returned the URL), `content`, and `content_error`.
With `include_content`, fetched pages also get `metadata`:

```json
"metadata": {
  "title": "Rust 2024 Edition: What's New", "description": "The Rust 2024 Edition stabilized in Rust 1.85…",
  "published": "2026-04-08", "modified": "2026-08-19", "author": "Rustify", "language": "en",
  "site_name": "Rustify", "canonical_url": "https://rustify.rs/articles/rust-2024-edition-whats-new",
  "final_url": "https://rustify.rs/articles/rust-2024-edition-whats-new", "word_count": 3137, "truncated": true
}
```

These come from `<title>`, meta/OpenGraph tags, JSON-LD, `<time>` and `<html lang>`. Missing fields are omitted.

**PDF results** are extracted too: text page by page (stopping at the character budget), with `title`, `author`
and `published` from the PDF's Info dictionary. PDFs larger than `CONTENT_MAX_PDF_MB` (10) get a `content_error`.
Big PDFs on slow hosts may need a longer `content_timeout_ms` (up to 30,000).
The response also lists `engines_used`, and adds `warnings` when an engine can't apply a filter.

Page-level extras:

```json
"correction": "rust programming language",
"suggestions": ["eiffel tower height", "eiffel tower tickets"],
"answer": {"kind": "abstract", "heading": "Eiffel Tower", "text": "The Eiffel Tower is a lattice tower…",
           "source": "Wikipedia", "url": "https://en.wikipedia.org/wiki/Eiffel_Tower"}
```

- `correction` is the engine's "did you mean" query. It comes free with the SERP (DuckDuckGo shows it in HTML;
  Bing and Brave render theirs with JavaScript only).
- `suggestions` and `answer` come from the engines' keyless JSON endpoints, the ones their own search boxes call.
  Google, Bing, Brave and DuckDuckGo serve suggestions; DuckDuckGo stands in for Yahoo and Mojeek.
  Answers come from DuckDuckGo's Instant Answer endpoint.
- Both extras run alongside the search, with a deadline of `EXTRAS_TIMEOUT_MS` (2000). A slow or failing extra
  never fails the search; it adds a `warnings` entry instead.

### Latency

- **Hedged fallback.** `auto` doesn't wait for a slow engine. After `HEDGE_DELAY_MS` (1200), the next
  engine starts in parallel and the first with results wins. A failing engine is replaced immediately.
  Losers are cancelled.
- **Merge grace.** `merge` waits at most `MERGE_GRACE_MS` (1500) after the first engine answers.
- **Content deadline.**
  - Page fetches share one deadline, so a slow site never holds the response.
  - Extraction runs on the blocking pool, capped at the core count.
  - Extracted text is cached by URL across searches (`CONTENT_CACHE_MB`).

### Other endpoints

| Route | Auth | Purpose |
|---|---|---|
| `GET /v1/engines` | yes | engines and whether each needs the browser |
| `GET /health` | no | liveness |
| `GET /ready` | no | readiness, browser state, cache size |
| `GET /metrics` | no | Prometheus counters (per-engine upstream / blocked / errors, cache hits) |

### Errors

Every error uses the same envelope, with a matching status code:

```json
{"error": {"code": "engine_blocked", "message": "...", "details": [ ... per-engine failures ... ]}}
```

`bad_request` 400 · `unauthorized` 401 · `rate_limited` 429 (+ `Retry-After`) ·
`engine_blocked` / `upstream_error` 502 · `overloaded` 503 · `timeout` 504

## Using it from an agent

Expose it as a tool. For example, a tool schema such as:

```json
{
  "name": "web_search",
  "description": "Search the web. Returns ranked results with title, url, snippet.",
  "input_schema": {
    "type": "object",
    "properties": {
      "query": {"type": "string"},
      "pages": {"type": "integer", "minimum": 1, "maximum": 3},
      "include_content": {"type": "boolean"}
    },
    "required": ["query"]
  }
}
```

The tool handler just forwards the input to `POST /v1/search`.

## Configuration

Every setting is an environment variable, documented in [`.env.example`](.env.example).
The ones that matter most:

| Variable | Why |
|---|---|
| `API_KEYS` | comma-separated keys (≥ 16 chars) |
| `PROXY_URL` | **Strongly recommended in production.** Engines throttle datacenter IPs; an HTTP/SOCKS5 egress proxy keeps the HTTP fast path working. |
| `FALLBACK_ENGINES` | order for `engine=auto` |
| `CACHE_TTL_SECS`, `CACHE_MAX_MB` | cache freshness and memory budget |
| `BROWSER_MAX_TABS` | browser concurrency (each tab ≈ 20–60 MB) |
| `RATE_LIMIT_PER_SEC`, `RATE_LIMIT_BURST` | per-key quota |

## Performance & scaling

- **Cache hits and coalesced requests** never leave the process. Measured on a 12-core laptop, with the
  load generator sharing the same CPUs, using a 20-result (~10 KB) JSON response, 256 concurrent
  connections and 0 errors:

  | Path | Throughput | p50 | p99 |
  |---|---|---|---|
  | cached search, uncompressed (`COMPRESSION=off` or no `Accept-Encoding`) | **~164,000 req/s** | 1.4 ms | 3.7 ms |
  | cached search, gzip/brotli fast level (default) | **~47,000 req/s** | 5.3 ms | 12.4 ms |
  | `/health` | ~220,000 req/s | 0.5 ms | 1.5 ms |

  Resident memory stayed around 60 MB under that load.
- **Cache misses** are bound by the upstream engines, not by this service. Measured with
  `examples/mock_upstream.rs`, which serves real SERP markup with 50 ms engine latency and
  30 ms result pages. Every request used a unique query, with 256 connections and 0 errors:

  | Scenario | Throughput | p50 | p99 |
  |---|---|---|---|
  | one engine | ~4,900 req/s | 52 ms | 55 ms |
  | `auto`, 3 pages | ~4,700 req/s | 54 ms | 63 ms |
  | `strategy=merge`, 5 engines | ~3,900 req/s | 65 ms | 80 ms |
  | one engine + content of top 3 | ~2,650 req/s | 95 ms | 108 ms |
  | first engine stalls 2 s: no hedging / default / `HEDGE_DELAY_MS=300` / `race` | – | 2003 / 1254 / 354 / 65 ms | – |

  The service adds about 2 ms over upstream latency. Memory is bounded by `CACHE_MAX_MB` +
  `CONTENT_CACHE_MB` plus the in-flight working set. With overload (`MAX_CONCURRENT_REQUESTS`) and rate
  limits hit, excess requests get immediate 503 / 429 responses (p99 < 10 ms).
  Browser misses cost a Chromium tab for one to a few seconds, which is why the browser is a fallback.
- **Stateless**: scale horizontally behind any load balancer. Each replica keeps its own cache.
  A shared Redis cache is on the roadmap.

Load test:

```bash
cargo run --release --example loadtest -- "http://127.0.0.1:8080/v1/search?q=rust&engine=brave" "$KEY" 256 10

# Cache-miss path against fake engines: `{n}` makes every query unique.
cargo run --release --example mock_upstream -- 127.0.0.1:9100 &
for e in BING BRAVE DUCKDUCKGO GOOGLE YAHOO MOJEEK; do export ENGINE_${e}_URL=http://127.0.0.1:9100/; done
CONTENT_ALLOW_PRIVATE_IPS=true BROWSER_ENABLED=false cargo run --release &
cargo run --release --example loadtest -- "http://127.0.0.1:8080/v1/search?q=rust+{n}&strategy=merge" "$KEY" 256 10
```

## Honest limitations

Scraping is an arms race. Engines change their markup and rate-limit aggressive clients. From a
flagged or datacenter IP you should expect:

| Engine | Plain HTTP | Headless browser |
|---|---|---|
| Bing | works; may soft-block (unrelated results), which is detected | works |
| Brave | works | works |
| DuckDuckGo | often captcha | works (page 1; deeper pages use HTTP form paging) |
| Yahoo | often refused | works |
| Mojeek | may show a JS challenge | – |
| Google | needs JS | needs a clean IP or a proxy |

Respect each engine's terms of service and keep request rates reasonable. The cache and per-key
limits exist partly for that reason.

## Development

```bash
cargo test                          # unit + parser fixtures + end-to-end API tests
cargo test --no-default-features    # HTTP-only build
cargo clippy --all-targets -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to add an engine, and [docs/PLAN.md](docs/PLAN.md) for the design.

## License

MIT
