FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && \
    cp /app/target/release/calod /tmp/calod && \
    strip /tmp/calod

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Calod" \
      org.opencontainers.image.description="Redis-compatible in-memory datastore" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home calod

WORKDIR /app

COPY --from=builder /tmp/calod /usr/local/bin/calod

RUN mkdir -p /app/data \
    && chown -R calod:calod /app

USER calod

EXPOSE 6379

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=12 \
  CMD sh -c "printf '*1\r\n$4\r\nPING\r\n' | nc -w 1 127.0.0.1 6379 | grep -q PONG"

ENTRYPOINT ["/usr/local/bin/calod"]
CMD ["--address", "0.0.0.0", "--port", "6379"]
