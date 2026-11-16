# syntax=docker/dockerfile:1

# ---- Build stage -----------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /app

# Dependency layer: build the dependencies against dummy sources so that this
# layer is only invalidated when Cargo.toml / Cargo.lock change.
# (rust-toolchain.toml is deliberately not copied: the image already ships the
# stable toolchain, and the file would make rustup download rustfmt/clippy.)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "" > src/lib.rs \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Real build. `touch` makes sure cargo sees the sources as newer than the
# dummy ones compiled above.
COPY migrations ./migrations
COPY src ./src
RUN touch src/lib.rs src/main.rs \
    && cargo build --release --locked

# ---- Runtime stage ---------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# ca-certificates: HTTPS calls to Spotify. curl: HEALTHCHECK. sqlite3: backups
# (`docker compose exec api sqlite3 ... ".backup ..."`).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin app \
    && mkdir -p /data \
    && chown app:app /data

COPY --from=builder /app/target/release/spotifast-stats-api /usr/local/bin/spotifast-stats-api

USER app
WORKDIR /data
ENV DATABASE_URL=sqlite:///data/spotifast.db \
    BIND_ADDR=0.0.0.0:8080
# Declared after the chown above so a fresh named volume inherits /data's owner.
VOLUME ["/data"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/spotifast-stats-api"]
