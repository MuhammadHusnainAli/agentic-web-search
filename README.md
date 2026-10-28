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
| `pages` | int | 1 | `1..=MAX_PAGES`, fetched concurrently |
| `mode` | string | `auto` | `auto` (HTTP → browser fallback), `http`, `browser` |
| `region` | string | `us-en` | `<country>-<language>` |
| `safe_search` | string | `moderate` | `off`, `moderate`, `strict` |
| `max_results` | int | – | truncate after merge (≤ 200) |
| `include_content` | bool | false | fetch and extract the text of top results |
| `content_limit` | int | 3 | how many results get `content` |

GET uses the same fields as query parameters: `/v1/search?q=rust&engine=brave&pages=2`.

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
- **Cache misses** are bound by the upstream engines, not by this service. Plain-HTTP misses cost a few
  milliseconds of CPU, mostly HTML parsing. Browser misses cost a Chromium tab for one to a few seconds,
  which is why the browser is a fallback.
- **Stateless**: scale horizontally behind any load balancer. Each replica keeps its own cache.
  A shared Redis cache is on the roadmap.

Load test:

```bash
cargo run --release --example loadtest -- "http://127.0.0.1:8080/v1/search?q=rust&engine=brave" "$KEY" 256 10
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
