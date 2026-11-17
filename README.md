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
| `POLL_INTERVAL_SECS` | `1200` | Minimum poll interval per user, in seconds: fast listeners, after a full page, while the pace is unknown. Below `300` it is refused at startup unless `POLL_ALLOW_FAST=true` (see [How plays are recorded](#how-plays-are-recorded)) |
| `POLL_MAX_INTERVAL_SECS` | `2400` | Maximum poll interval per user, in seconds: slow and idle listeners (must be >= `POLL_INTERVAL_SECS`) |
| `POLL_ALLOW_FAST` | `false` | Accept a `POLL_INTERVAL_SECS` below 300 (tests, debugging): it only spends quota |
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
Pull requests and pushes to other branches only build it (no push), which is
how the Dockerfile is tested. Pushes to `main` publish
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
| GET | `/health` | none | `{"status":"ok"}` when the database answers, 503 otherwise |
| GET | `/auth/login` | none | Redirects the browser to Spotify's consent page (rate limited per IP); optional `app_redirect` and `app_state` for [desktop apps](#desktop-apps) |
| GET | `/auth/callback` | none | Spotify redirects here; HTML page with the token in a browser, `{"token", "user"}` JSON otherwise, or a `302` to the desktop app's `app_redirect` |
| GET | `/me` | Bearer | `id`, `spotify_id`, `display_name` |
| GET | `/me/status` | Bearer | Poller state: last successful poll, next poll, current interval, rate limit, last error, play count |
| DELETE | `/me` | Bearer | Deletes the account, its plays and its tokens |
| GET | `/auth/tokens` | Bearer | Lists your API tokens (`id`, `created_at`, `last_used_at`, `current`) |
| POST | `/auth/token` | Bearer | Creates a new API token, returned once |
| DELETE | `/auth/token/{id}` | Bearer | Revokes one of your tokens (the current one included) |
| GET | `/stats/summary` | Bearer | Plays, listening time, distinct tracks/artists/albums, first and last play |
| GET | `/stats/top/tracks` | Bearer | Top tracks |
| GET | `/stats/top/artists` | Bearer | Top artists |
| GET | `/stats/top/albums` | Bearer | Top albums |
| GET | `/stats/listening-time` | Bearer | Plays and listening time per `bucket` (`day`, `week`, `month`), UTC |
| GET | `/stats/hours` | Bearer | Plays per hour of day (24) and per weekday (7), in time zone `tz` |
| GET | `/stats/streaks` | Bearer | Current and longest run of consecutive listening days, in time zone `tz` |
| GET | `/stats/compare?with=<user_id>` | Bearer | Artists and tracks in common with another user of the instance, and a similarity score |
| GET | `/history` | Bearer | Plays, newest first |
| GET | `/now-playing` | Bearer | What you are listening to right now (cached 30 s); 204 when nothing plays |
| POST | `/import` | Bearer | Imports a Spotify extended streaming history file, or the whole export zip |

Authenticated routes expect `Authorization: Bearer <token>`. Errors are JSON:
`{"error": "..."}`.

### Getting a token

Open `https://<your domain>/auth/login` in a browser and accept on Spotify. The
callback page shows your API token: copy it, it is shown only once. You can
create more tokens with `POST /auth/token`, list them with `GET /auth/tokens`
and revoke them with `DELETE /auth/token/{id}`. Each Spotify account that logs
in becomes a separate user (restrict who may log in with
`ALLOWED_SPOTIFY_IDS`), and its plays are recorded from then on.

Users who logged in before `/now-playing` existed must log in again once to
use it (it needs the `user-read-currently-playing` scope); play tracking keeps
working without that.

### Desktop apps

A desktop app (such as Spotifast) can receive the token itself instead of the
user copying it from the callback page, with the loopback redirect of
[RFC 8252](https://www.rfc-editor.org/rfc/rfc8252#section-7.3):

1. Listen on a loopback port, for example `http://127.0.0.1:53682/callback`.
2. Open the system browser on
   `https://<your domain>/auth/login?app_redirect=<URL-encoded loopback URL>&app_state=<random value>`.
3. After the Spotify consent, `/auth/callback` answers `302` to
   `<app_redirect>?token=<token>&state=<app_state>` instead of showing the
   token page (with `&` instead of `?` when `app_redirect` already has a
   query; values are URL-encoded). If the login fails after the state check
   (consent refused, Spotify error, account not allowed...), it redirects to
   `<app_redirect>?error=<message>&state=<app_state>` instead.
4. Check that `state` is the value you sent, then keep `token`.

| Parameter | Rule |
| --- | --- |
| `app_redirect` | Optional. An `http` URL whose host is `127.0.0.1`, `[::1]` or `localhost`, with an explicit port (not the default `80`), any path and query, no user info and no fragment, at most 512 characters. Anything else gets `400 {"error":"app_redirect must be a loopback http URL"}`, so a token can only be handed to a program on the user's own machine. |
| `app_state` | Optional. Up to 128 characters of `A-Z a-z 0-9 _ -`, otherwise `400`. Sent back unchanged as `state` (omitted when not given). Only used along with `app_redirect`. |

An empty value counts as absent. Both are kept in the signed state cookie, so
they cannot be changed between login and callback. A missing, invalid or
expired `state` at the callback still gets the plain JSON error: without a
verified cookie there is no trusted `app_redirect` to send it to. Without
`app_redirect`, the login works as described above.

### How plays are recorded

Spotify only returns the 50 most recent plays and the app's quota is shared
by every user, so the poller calls Spotify as rarely as it can without ever
letting that page overflow between two polls:

- one `recently-played` call per user and per poll, only for plays newer than
  the last one recorded (the `after` cursor);
- each user's pace is measured on every page holding at least two plays: the
  time between its oldest and newest play divided by the number of plays, in
  seconds per play, smoothed by an exponential moving average (the newest page
  counts for half) and stored in the database;
- the next poll comes after 25 plays at that pace, so the page is about half
  full when it is read, bounded by `POLL_INTERVAL_SECS` (default 20 min) and
  `POLL_MAX_INTERVAL_SECS` (default 40 min), ±10 % jitter; while the pace is
  not known yet, the minimum is used;
- a full page (50 plays) means plays may have fallen out of it: the next poll
  is at the minimum, at once, and a warning is logged;
- a poll without new plays (idle user) schedules the next one at the maximum;
- when Spotify answers 429, that user is not called again before the
  `Retry-After` delay, even across restarts (it is stored in the database);
- the access token is refreshed only when it expires; if Spotify revokes it,
  the user must log in again (`/me/status` says so in `last_error`).

`GET /me/status` shows where your account stands.

Why a 20 min minimum, and never less than 5 min: Spotify only counts a play
after 30 s of listening, so 50 plays take at least 50 × 30 s = 25 min. A poll
every 20 min (22 at most with the jitter) cannot miss a play, even for someone
skipping tracks every 30 s. Every 5 min the page would be at most a fifth full:
a shorter minimum only spends the shared quota, so a `POLL_INTERVAL_SECS` below
300 is refused at startup unless `POLL_ALLOW_FAST=true`. The maximum (40 min)
is used for paces above 96 s per play, where 25 plays take more than 40 min, and
after an idle poll; then only someone skipping a track every 30 to 48 s for 40
minutes straight could fill the page before the next poll, and the full-page
warning would show it.

### Query parameters

- Period (all `/stats/*` routes except `/stats/streaks`): `range` = `4w`, `6m`,
  `1y` or `lifetime` (default), or `from` / `to` (unix milliseconds or RFC
  3339, `to` exclusive). `range` cannot be combined with `from` / `to`.
- Time zone (`/stats/hours`, `/stats/streaks`): `tz`, an IANA name such as
  `Europe/Paris` (default `UTC`).
- Top routes: `sort` = `plays` (default) or `duration`, `limit` (default 50,
  max 200), `offset`.
- `/stats/compare`: `with` (the other user's id), `limit` (default 20, max
  100). `similarity` is the weighted Jaccard index of the two users' play
  counts per artist, between 0 and 1.
- `/history`: `limit` and `before` (a `played_at` cursor); the response gives
  `next_before` for the next page.

```sh
curl -H "Authorization: Bearer $TOKEN" \
  "https://<your domain>/stats/hours?range=6m&tz=Europe/Paris"
```

### Importing your history

Request your **Extended streaming history** from Spotify's privacy page. Send
the zip Spotify gives you as it is (200 MB max, 1 GB once uncompressed):

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/zip" \
  --data-binary @my_spotify_data.zip \
  https://<your domain>/import
```

Every `Streaming_History_Audio_*.json` file in the archive is imported; other
files (video history, PDF…) are listed in `ignored_files`. The answer gives a
report per file and `totals`. If one file is invalid, nothing is imported.

A single file can also be sent on its own (64 MB max):

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  --data-binary @Streaming_History_Audio_2021-2022_3.json \
  https://<your domain>/import
```

Entries under 30 seconds, podcasts and local files are skipped, and so are
listens already recorded by the poller. Importing the same data twice adds
nothing.
