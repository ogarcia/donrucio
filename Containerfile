# syntax=docker/dockerfile:1.28
#
# Don Rucio — a lightweight Telegram help bot for Rucio.
#
# Two build paths, selected by the BUILDER arg:
#   - builder  (default): compile from source inside the image. Good for a
#              local `podman build .` with no extra setup.
#   - prebuilt (CI): inject a binary already compiled by the workflow into
#              dist/<arch>/donrucio, so the heavy Rust build cache lives in CI
#              (Swatinem/rust-cache) instead of the Docker build.
#
# Local build:
#   podman build -t donrucio:dev .
#
# CI build (binaries pre-compiled into dist/amd64|arm64, matching TARGETARCH):
#   podman build --build-arg BUILDER=prebuilt .
#
# Runtime: needs TELEGRAM_BOT_TOKEN and GROQ_API_KEY in the environment.
#   podman run -e TELEGRAM_BOT_TOKEN=... -e GROQ_API_KEY=... donrucio:dev

# Declared before the first FROM so they apply to every stage.
ARG BUILDER=builder
ARG ALPINE_VERSION=3.24
ARG RUST_VERSION=1

# ── Stage 1a: compile from source (default local path) ───────────────────────
FROM docker.io/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder

# musl-dev: C toolchain bits ring needs (rustls via ureq).
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY . .
RUN cargo build --release --locked && \
    cp target/release/donrucio /usr/bin/donrucio && \
    strip /usr/bin/donrucio

# ── Stage 1b: pre-built binary injected from CI (dist/ in build context) ──────
# TARGETARCH (amd64/arm64) is set automatically by buildx per target platform.
FROM scratch AS prebuilt
ARG TARGETARCH
COPY --chmod=0755 dist/${TARGETARCH}/donrucio /usr/bin/donrucio

# ── Stage 1c: indirection — points to 'builder' or 'prebuilt' via build arg ──
FROM ${BUILDER} AS bins

# ── Stage 2: runtime ─────────────────────────────────────────────────────────
FROM docker.io/alpine:${ALPINE_VERSION} AS runtime

RUN apk add --no-cache ca-certificates && \
    addgroup -S -g 10001 donrucio && \
    adduser  -S -G donrucio -u 10001 -h /home/donrucio donrucio

COPY --from=bins /usr/bin/donrucio /usr/bin/donrucio

USER donrucio
WORKDIR /home/donrucio

ENTRYPOINT ["/usr/bin/donrucio"]
