# Builds the Redis tcl-suite runner image: calod release binary + tcl +
# the Redis test suite at a pinned tag. Build context is the calod/ crate
# root. Driven by scripts/tcl-suite/run_tcl_suite.sh.

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

# Pinned Redis tag whose tests/ tree defines the parity baseline.
ARG REDIS_TAG=7.4.2
ENV REDIS_TAG=${REDIS_TAG}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       tcl tcl-tls git ca-certificates procps \
    && rm -rf /var/lib/apt/lists/*

# Shallow clone at the exact tag; only the test suite is used.
RUN git clone --depth 1 --branch "${REDIS_TAG}" \
      https://github.com/redis/redis.git /opt/redis

COPY --from=builder /tmp/calod /usr/local/bin/calod
COPY scripts/tcl-suite/run_suites.sh /opt/tcl-runner/run_suites.sh
RUN chmod +x /opt/tcl-runner/run_suites.sh

WORKDIR /opt/redis/tests

ENTRYPOINT ["/opt/tcl-runner/run_suites.sh"]
