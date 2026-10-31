FROM rust:1-bookworm AS builder

WORKDIR /app

# Workspace layout: the root package plus the five member crates.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release -p calod && \
    cp /app/target/release/calod /tmp/calod && \
    strip /tmp/calod

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Calod" \
      org.opencontainers.image.description="Durable action ledger for AI agents (RESP store included)" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home calod

WORKDIR /app

COPY --from=builder /tmp/calod /usr/local/bin/calod

RUN mkdir -p /app/data \
    && chown -R calod:calod /app

USER calod

# Serve the ledger API on all interfaces inside the container (the
# default bind is loopback, which Docker port publishing cannot reach).
ENV CALOD_API__LISTEN_ADDR=0.0.0.0:7379

# 6379 = RESP (frozen compatibility interface), 7379 = ledger REST API.
EXPOSE 6379 7379

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=12 \
  CMD curl -fsS http://127.0.0.1:7379/v1/health/ready || exit 1

ENTRYPOINT ["/usr/local/bin/calod"]
CMD ["--address", "0.0.0.0", "--port", "6379"]
