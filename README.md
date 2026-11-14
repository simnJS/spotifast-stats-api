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

| Method | Path | Auth | Description |
| --- | --- | --- | --- |
| GET | `/health` | none | Returns `{"status":"ok"}` |
| GET | `/auth/login` | none | Redirects the browser to Spotify's consent page |
| GET | `/auth/callback` | none | Spotify redirects here; returns `{"token", "user"}` |
| GET | `/me` | Bearer | Returns `id`, `spotify_id`, `display_name` |
| GET | `/stats/summary` | Bearer | Plays, listening time, distinct tracks/artists/albums, first and last play |
| GET | `/stats/top/tracks` | Bearer | Top tracks |
| GET | `/stats/top/artists` | Bearer | Top artists |
| GET | `/stats/top/albums` | Bearer | Top albums |
| GET | `/stats/listening-time` | Bearer | Plays and listening time per `bucket` (`day`, `week`, `month`), UTC |
| GET | `/history` | Bearer | Plays, newest first |
| POST | `/import` | Bearer | Imports one Spotify extended streaming history file |

Authenticated routes expect `Authorization: Bearer <token>`. Errors are JSON:
`{"error": "..."}`.

### Getting a token

Open `http://<host>:8080/auth/login` in a browser and accept on Spotify. The
callback page shows a JSON document; keep its `token`, it is shown only once
(log in again to get a new one). Each Spotify account that logs in becomes a
separate user, and its plays are recorded from then on, every
`POLL_INTERVAL_SECS`.

### Query parameters

- Period (all `/stats/*` routes): `range` = `4w`, `6m`, `1y` or `lifetime`
  (default), or `from` / `to` (unix milliseconds or RFC 3339, `to` exclusive).
  `range` cannot be combined with `from` / `to`.
- Top routes: `sort` = `plays` (default) or `duration`, `limit` (default 50,
  max 200), `offset`.
- `/history`: `limit` and `before` (a `played_at` cursor); the response gives
  `next_before` for the next page.

```sh
curl -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:8080/stats/top/artists?range=6m&limit=10"
```

### Importing your history

Request your **Extended streaming history** from Spotify's privacy page, then
send each `Streaming_History_Audio_*.json` file, one request per file (64 MB
max):

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  --data-binary @Streaming_History_Audio_2021-2022_3.json \
  http://127.0.0.1:8080/import
```

Entries under 30 seconds, podcasts and local files are skipped, and so are
listens already recorded by the poller. Importing the same file twice adds
nothing.
