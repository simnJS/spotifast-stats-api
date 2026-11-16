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
| `POLL_INTERVAL_SECS` | `120` | Minimum (and initial) interval between play polls, per user |
| `POLL_MAX_INTERVAL_SECS` | `600` | Maximum poll interval for inactive users (must be >= `POLL_INTERVAL_SECS`) |
| `ALLOWED_SPOTIFY_IDS` | unset | Comma-separated Spotify user ids allowed to log in; unset = anyone Spotify lets through |
| `TRUST_X_FORWARDED_FOR` | `false` | Take the client IP from `X-Forwarded-For`; set `true` only behind a reverse proxy |
| `AUTH_RATE_LIMIT_PER_MIN` | `20` | Requests per minute and per IP allowed on `/auth/login` and `/auth/callback` |
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
docker compose up -d
curl http://127.0.0.1:8080/health
```

`docker-compose.yml` uses the published image
`ghcr.io/simnjs/spotifast-stats-api:latest` and falls back to building the local
`Dockerfile` (`docker compose up -d --build`). The container runs as a non-root
user, the database lives in the `spotifast-data` named volume mounted at
`/data`, and the port is published on `127.0.0.1:8080` only: put a reverse proxy
in front of it. The image has a `HEALTHCHECK` on `GET /health`.

## Deploy behind a reverse proxy (HTTPS)

Spotify only accepts an `https` redirect URI, except for `127.0.0.1`. So on a
server, put a TLS-terminating proxy in front of the container. Minimal
[Caddy](https://caddyserver.com) example (`Caddyfile`, certificates are obtained
automatically):

```
stats.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

In `.env`:

```sh
SPOTIFY_REDIRECT_URI=https://stats.example.com/auth/callback
TRUST_X_FORWARDED_FOR=true   # Caddy sets X-Forwarded-For; needed for per-IP rate limiting
```

Use the same redirect URI in the Spotify dashboard. Only set
`TRUST_X_FORWARDED_FOR=true` if the API is reachable exclusively through the
proxy, otherwise clients can spoof their IP.

### Spotify development mode

A new Spotify app starts in development mode:

- the app owner needs a Spotify Premium account;
- at most 5 users can use the app;
- each user must be added by hand (name and Spotify email) in the app's
  dashboard under **User Management**, otherwise login is refused.

Use `ALLOWED_SPOTIFY_IDS` to restrict logins on your side as well.

### Getting the image onto the server

The image `ghcr.io/simnjs/spotifast-stats-api` only exists once the
`Docker` workflow has run on a push to `main` (or on a `v*` tag). Until then,
`docker compose pull` finds nothing; use `docker compose up -d --build` instead.

A package published to GHCR from a personal account is created **private** by
default, even if the repository is public (visibility is not inherited from the
repository). Without a fix, `docker compose pull` on the server fails with
`denied` / `unauthorized`. After the first successful publish, either:

- make the package public: GitHub profile → **Packages** → `spotifast-stats-api`
  → **Package settings** → **Danger Zone** → **Change visibility** → **Public**
  (this cannot be undone; public images can be pulled anonymously); or
- keep it private and log in on the server with a personal access token that has
  the `read:packages` scope:

  ```sh
  echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-username> --password-stdin
  ```

See GitHub's [Configuring a package's access control and visibility](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility).

### Updating

```sh
docker compose pull
docker compose up -d
```

The volume (and so the database) is kept; migrations run at startup.

### Backing up the database

The image ships `sqlite3`. `.backup` gives a consistent copy even while the API
is running:

```sh
docker compose exec api sqlite3 /data/spotifast.db ".backup '/data/backup.db'"
docker compose cp api:/data/backup.db ./spotifast-$(date +%F).db
docker compose exec api rm /data/backup.db
```

To restore, stop the stack and copy the file back into the volume, for example
with a one-off container (adjust the volume name, see `docker volume ls`):

```sh
docker compose down
docker run --rm -v spotifast-stats-api_spotifast-data:/data -v "$PWD":/b \
  debian:bookworm-slim sh -c 'cp /b/spotifast.db /data/spotifast.db && chown 10001:10001 /data/spotifast.db'
docker compose up -d
```

## Publishing the image

`.github/workflows/docker.yml` runs fmt, clippy and tests, then builds the image.
Pull requests only build it (no push). Pushes to `main` publish
`ghcr.io/<owner>/spotifast-stats-api` tagged `latest` and with the commit sha;
`v*` tags also publish the version (e.g. `v1.2.3` gives `1.2.3`).

## Create the Spotify developer app

1. Go to <https://developer.spotify.com/dashboard> and click **Create app**.
2. Set the **Redirect URI** to the value of `SPOTIFY_REDIRECT_URI`
   (for example `http://127.0.0.1:8080/auth/callback`, or an `https` URL in
   production). It must match exactly.
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
