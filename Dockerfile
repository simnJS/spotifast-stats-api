# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin app \
    && mkdir -p /data \
    && chown app:app /data
COPY --from=builder /app/target/release/spotifast-stats-api /usr/local/bin/spotifast-stats-api
USER app
WORKDIR /data
ENV DATABASE_URL=sqlite:///data/spotifast.db \
    BIND_ADDR=0.0.0.0:8080
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/spotifast-stats-api"]
