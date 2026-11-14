# spotifast-stats-api

A multi-user, stats.fm-like HTTP API written in Rust (Axum + SQLite). It tracks
each user's Spotify plays and serves listening stats.

## Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `DATABASE_URL` | `sqlite://data/spotifast.db` | SQLite database (file created if missing) |
| `BIND_ADDR` | `0.0.0.0:8080` | Address the server listens on |
| `SPOTIFY_CLIENT_ID` | required | Spotify app client id |
| `SPOTIFY_CLIENT_SECRET` | required | Spotify app client secret |
| `SPOTIFY_REDIRECT_URI` | required | OAuth redirect URI, must match the Spotify app |
| `POLL_INTERVAL_SECS` | `120` | Interval between play polls |
| `SPOTIFY_API_BASE` | `https://api.spotify.com/v1` | Spotify Web API base (override for tests) |
| `SPOTIFY_ACCOUNTS_BASE` | `https://accounts.spotify.com` | Spotify accounts base (override for tests) |
| `RUST_LOG` | `info` | Log filter |

A `.env` file is loaded automatically. See `.env.example`.

## Run locally

```sh
cp .env.example .env   # then edit the Spotify values
cargo run
curl http://127.0.0.1:8080/health
```

Checks: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`

## Run with Docker

```sh
cp .env.example .env   # then edit the Spotify values
docker compose up --build
```

The database lives in the `spotifast-data` named volume mounted at `/data`.

## Create the Spotify developer app

1. Go to <https://developer.spotify.com/dashboard> and click **Create app**.
2. Set the **Redirect URI** to the value of `SPOTIFY_REDIRECT_URI`
   (for example `http://127.0.0.1:8080/auth/callback`). It must match exactly.
3. Copy the **Client ID** and **Client secret** into `.env`.

## Endpoints

| Method | Path | Auth | Status |
| --- | --- | --- | --- |
| GET | `/health` | none | available, returns `{"status":"ok"}` |
| GET | `/me` | Bearer | available, returns `id`, `spotify_id`, `display_name` |
| | Spotify auth (login/callback) | | coming |
| | Stats endpoints | Bearer | coming |
| | History import | Bearer | coming |

Authenticated routes expect `Authorization: Bearer <token>`. Errors are JSON:
`{"error": "..."}`.
