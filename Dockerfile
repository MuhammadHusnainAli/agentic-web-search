#
# Two runtime targets:
#   full (default)  Debian slim + Chromium: every engine + browser fallback
#   lite            distroless + HTTP-only binary (no browser): tiny & fast
#
#   docker build -t agentic-web-search .
#   docker build -t agentic-web-search:lite --target lite .
#
# Works with both BuildKit and the legacy builder (no cache mounts needed):
# dependencies are compiled in their own layer from a stub crate, so editing
# `src/` only recompiles this crate.

ARG RUST_VERSION=1
# `release` (fat LTO, for published images) or `ci` (no LTO, ~8x faster
# rebuilds, for CI smoke tests). The binary lands in target/<profile>/.
ARG CARGO_PROFILE=release

# ---------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-slim-bookworm AS build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/app examples \
    && echo 'fn main() {}' > src/app/main.rs \
    && touch src/lib.rs \
    && echo 'fn main() {}' > examples/loadtest.rs

FROM build-base AS build-full
ARG CARGO_PROFILE
RUN cargo build --profile ${CARGO_PROFILE} --locked
COPY src ./src
RUN touch src/lib.rs src/app/main.rs \
    && cargo build --profile ${CARGO_PROFILE} --locked \
    && cp target/${CARGO_PROFILE}/agentic-web-search /agentic-web-search

FROM build-base AS build-lite
ARG CARGO_PROFILE
RUN cargo build --profile ${CARGO_PROFILE} --locked --no-default-features
COPY src ./src
RUN touch src/lib.rs src/app/main.rs \
    && cargo build --profile ${CARGO_PROFILE} --locked --no-default-features \
    && cp target/${CARGO_PROFILE}/agentic-web-search /agentic-web-search

# ---------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS lite
COPY --from=build-lite /agentic-web-search /usr/local/bin/agentic-web-search
ENV HOST=0.0.0.0 \
    PORT=8080 \
    LOG_FORMAT=json \
    BROWSER_ENABLED=false
EXPOSE 8080
USER nonroot
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/agentic-web-search", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/agentic-web-search"]

# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS full
RUN apt-get update \
    && apt-get install -y --no-install-recommends -o Acquire::Retries=5 chromium ca-certificates fonts-liberation tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin app
COPY --from=build-full /agentic-web-search /usr/local/bin/agentic-web-search
ENV HOST=0.0.0.0 \
    PORT=8080 \
    LOG_FORMAT=json \
    BROWSER_ENABLED=true \
    BROWSER_EXECUTABLE=/usr/bin/chromium
EXPOSE 8080
USER app
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/agentic-web-search", "healthcheck"]
# tini reaps Chromium's child processes and forwards SIGTERM for graceful shutdown.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/agentic-web-search"]
