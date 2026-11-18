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
| `METADATA_CALLS_PER_TICK` | `4` | Spotify lookups the metadata backfill may make every 15 s (ids, images and genres of imported tracks and of artists, see [Spotify metadata](#spotify-metadata)); `0` turns it off |
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
| GET | `/auth/login` | none | Redirects the browser to Spotify's consent page (rate limited per IP); optional `app_redirect` and `app_state` for [desktop apps](#desktop-apps); `503` while [Spotify rate limits the app](#when-spotify-rate-limits-the-app) |
| GET | `/auth/callback` | none | Spotify redirects here; HTML page with the token in a browser, `{"token", "user"}` JSON otherwise, or a `302` to the desktop app's `app_redirect`; `503` while [Spotify rate limits the app](#when-spotify-rate-limits-the-app) |
| GET | `/me` | Bearer | `id`, `spotify_id`, `display_name` |
| GET | `/me/status` | Bearer | Poller state: last successful poll, next poll, current interval and its bounds, measured pace, end of the Spotify rate limit, last error, play count (see [below](#me-status)) |
| DELETE | `/me` | Bearer | Deletes the account, its plays and its tokens |
| GET | `/auth/tokens` | Bearer | Lists your API tokens (`id`, `created_at`, `last_used_at`, `current`) |
| POST | `/auth/token` | Bearer | Creates a new API token, returned once |
| DELETE | `/auth/token/{id}` | Bearer | Revokes one of your tokens (the current one included) |
| GET | `/stats/summary` | Bearer | Plays, listening time, distinct tracks/artists/albums, first and last play |
| GET | `/stats/top/tracks` | Bearer | Top tracks, with rank, rank over the period before, ids, artists, album and cover (see [Tops](#tops)) |
| GET | `/stats/top/artists` | Bearer | Top artists, with rank, rank over the period before, id and image |
| GET | `/stats/top/albums` | Bearer | Top albums, with rank, rank over the period before, id, artists and cover |
| GET | `/stats/top/genres` | Bearer | Top genres of the artists played, with rank and rank over the period before |
| GET | `/stats/item/{kind}/{spotify_id}` | Bearer | One track, artist or album (`kind` = `track`, `artist`, `album`): plays, time, first and last play, lifetime and 4-week rank, top tracks (see [Item](#item)) |
| GET | `/stats/listening-time` | Bearer | Plays and listening time per `bucket` (`day`, `week`, `month`) in time zone `tz` |
| GET | `/stats/hours` | Bearer | Plays per hour of day (24) and per weekday (7), in time zone `tz` |
| GET | `/stats/streaks` | Bearer | Current and longest run of consecutive listening days, in time zone `tz` |
| GET | `/stats/compare?with=<user_id>` | Bearer | Artists and tracks in common with another user of the instance, and a similarity score |
| GET | `/history` | Bearer | Plays, newest first |
| GET | `/now-playing` | Bearer | What you are listening to right now (cached 30 s); 204 when nothing plays; while Spotify rate limits the app, the last answer, or `503` with `Retry-After` |
| POST | `/import` | Bearer | Imports a Spotify extended streaming history file (64 MB max), or the whole export zip (512 MB max) |

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
- when Spotify answers 429, nobody is polled before the end of the block
  (see [below](#when-spotify-rate-limits-the-app));
- the access token is refreshed only when it expires; if Spotify revokes it,
  the user must log in again (`/me/status` says so in `last_error`).

<a id="me-status"></a>`GET /me/status` shows where your account stands (times are unix
milliseconds, `null` when unknown or not applicable):

| Field | Meaning |
| --- | --- |
| `last_poll_ok_at` | Last poll that succeeded, with or without new plays |
| `next_poll_at` | When the poller reads Spotify next for you: the scheduled poll, or the end of the Spotify rate limit when that is later (`null`: as soon as possible) |
| `poll_interval_secs` | Current interval between two polls, in seconds (`null` before the first poll) |
| `poll_min_interval_secs`, `poll_max_interval_secs` | Its bounds, `POLL_INTERVAL_SECS` and `POLL_MAX_INTERVAL_SECS` |
| `secs_per_play` | Your measured listening pace, in seconds per play, smoothed, to a tenth (`null` until measured) |
| `rate_limited_until` | End of the app-wide block while Spotify rate limits the app (`null` otherwise) |
| `last_error`, `last_error_at` | Last poll failure and when, cleared by the next success |
| `last_played_at` | Newest play recorded by the poller (its cursor) |
| `plays_count` | Plays recorded for you (poller and imports) |

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

### When Spotify rate limits the app

Spotify's quota belongs to the app, not to a user, so a 429 on any call (the
poller, `/now-playing`, the metadata backfill, a token refresh, a login) blocks
the whole app: no call
at all is sent to Spotify, for anyone, before `Retry-After` has passed (without
a usable `Retry-After`: 1 min, doubled while the 429s follow one another, up to
1 h). The end of the block is stored in the database, so it holds across
restarts, and the log says so at startup. Meanwhile:

- the poller skips its cycles; the users left are polled once the block is
  over, and the cursor guarantees no play readable then is missed;
- `/now-playing` answers its last cached answer, or `503` with `Retry-After`;
- the [metadata backfill](#spotify-metadata) sends nothing; it resumes after
  the block;
- `/auth/login`, and `/auth/callback` when Spotify answers 429 during the
  login (token exchange or profile), answer `503` with `Retry-After` and
  « Spotify limite l’app jusqu’à HH:MM UTC » (the end, rounded up to the
  minute, with the date when it is a day or more away): an HTML page for a
  browser, `{"error": "..."}` otherwise. Nothing on it sends the browser back to
  Spotify, so it cannot loop: log in again after that time. A desktop app's
  callback gets the same message as `error` on its `app_redirect`.

### Query parameters

- Period (all `/stats/*` routes except `/stats/streaks`): `range` = `4w`, `6m`,
  `1y` or `lifetime` (default), or `from` / `to` (unix milliseconds or RFC
  3339, `to` exclusive). `range` cannot be combined with `from` / `to`.
- Time zone (`/stats/listening-time`, `/stats/hours`, `/stats/streaks`): `tz`,
  an IANA name such as `Europe/Paris` (default `UTC`). Days, weeks (from
  Monday) and months start at the zone's midnight, daylight saving time
  included; `listening-time` gives each bucket's first instant as `start`
  (unix ms) and leaves out empty buckets.
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

### What a play counts for

A play counts for its track, for **every artist credited on it** (a featured
artist too), for its album, and once for each genre of those artists. A track
whose artists and album are not known yet (imported, not yet looked up, see
[Spotify metadata](#spotify-metadata)) counts for the artist and album names
of the history file: for the stored artist (album) of that name when there is
one, else for a bare name without an id. `/stats/summary`'s
`distinct_artists` and `distinct_albums` follow the same rule as the tops.

### Tops

`/stats/top/{tracks,artists,albums,genres}` take the period, `sort`, `limit`
and `offset`, and answer `{"items": [...]}`. Every item has:

| Field | Meaning |
| --- | --- |
| `rank` | Its place in the whole ranking (1 = first), in the `sort` order; `offset` + its index + 1 |
| `previous_rank` | Its place, same order, over the period of the same length that ends where this one starts (`range=4w`: the 28 days before; `6m`/`1y`: as many days before; an open `to` stands for now). `null` when it was not played then (new), and always for `lifetime` or a period without `from` |
| `plays`, `ms_played` | Plays and listening time in the period |

Up: `previous_rank > rank`; down: `previous_rank < rank`; new: `previous_rank`
null while the period has a start.

Tracks:

```json
{
  "rank": 1, "previous_rank": 3,
  "spotify_id": "4uLU6hMCjMI75M1A2tKUQC", "uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
  "name": "Song", "image_url": "https://i.scdn.co/image/…", "duration_ms": 213000,
  "artists": [{ "spotify_id": "0OdUWJ0sBjDrqHygGUXeCF", "name": "Artist" },
              { "spotify_id": null, "name": "Not looked up yet" }],
  "album": { "spotify_id": "1A2GTWGtFfWp7KSQTwWOyo", "name": "Album", "image_url": "https://i.scdn.co/image/…" },
  "artist_name": "Artist", "album_name": "Album",
  "plays": 42, "ms_played": 8946000
}
```

`image_url` is the album's cover. `artists` are the credited artists, main
one first. `artist_name` and `album_name` are the former fields, kept.

Artists:

```json
{
  "rank": 1, "previous_rank": null,
  "spotify_id": "0OdUWJ0sBjDrqHygGUXeCF", "uri": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF",
  "name": "Artist", "image_url": "https://i.scdn.co/image/…", "artist_name": "Artist",
  "plays": 120, "ms_played": 25200000
}
```

Albums:

```json
{
  "rank": 2, "previous_rank": 1,
  "spotify_id": "1A2GTWGtFfWp7KSQTwWOyo", "uri": "spotify:album:1A2GTWGtFfWp7KSQTwWOyo",
  "name": "Album", "image_url": "https://i.scdn.co/image/…",
  "artists": [{ "spotify_id": "0OdUWJ0sBjDrqHygGUXeCF", "name": "Artist" }],
  "album_name": "Album", "artist_name": "Artist",
  "plays": 64, "ms_played": 13440000
}
```

Genres:

```json
{ "genre": "french pop", "rank": 1, "previous_rank": 2, "plays": 310, "ms_played": 65100000 }
```

What can be `null`, and when:

| Field | `null` when |
| --- | --- |
| `previous_rank` | not played in the period before, or no period before (`lifetime`) |
| `spotify_id`, `uri` (artist, album, and in `artists` / `album`) | an imported track not looked up on Spotify yet, or that Spotify does not know, and no stored artist (album) has its name |
| `image_url` (track, album) | album unknown as above, or Spotify gives no cover |
| `image_url` (artist) | artist not looked up yet, or Spotify gives no image |
| `duration_ms` | an imported track not looked up yet, or that Spotify does not know |

A track's own `spotify_id` and `uri` are never null. Genres come from
Spotify's artist `genres`, which Spotify marks as deprecated and often leaves
empty: `/stats/top/genres` then has few or no items (it is never made up).

### Item

`GET /stats/item/{kind}/{spotify_id}`, `kind` = `track`, `artist` or `album`,
looks at the caller's whole history:

```json
{
  "plays": 120, "ms_played": 25200000,
  "first_played_at": 1609459200000, "last_played_at": 1800000000000,
  "rank_lifetime": 3, "rank_4w": 12,
  "top_tracks": [
    { "spotify_id": "4uLU6hMCjMI75M1A2tKUQC", "name": "Song", "plays": 42, "ms_played": 8946000 }
  ]
}
```

`rank_lifetime` and `rank_4w` are its ranks (by plays) in the lifetime and
last-28-days tops of its kind; `rank_4w` is `null` when it was not played in the
last 28 days. `top_tracks` are an artist's or an album's 5 most played tracks
(empty for a track). Times are unix ms. 404 when the caller never played it (or
it is only known by name, before its lookup); 400 for another `kind`.

### Spotify metadata

Ids, images and genres are stored in the database (`artists`, `albums`,
`track_artists`, `album_artists`, `artist_genres`):

- a polled play brings them: Spotify's `recently-played` gives the track's
  artists with their ids and its album with its id and cover;
- an imported play only has names, so after each poller cycle a backfill looks
  up a few items on Spotify: tracks without metadata first, the most played
  (all users) first, then artists without their image and genres. At most
  `METADATA_CALLS_PER_TICK` lookups every 15 s (default 4), and **none while
  Spotify rate limits the app**; a 429 it gets blocks the whole app like any
  other (see [below](#when-spotify-rate-limits-the-app));
- it asks for 50 items per call (`/tracks?ids=`, `/artists?ids=`) while Spotify
  serves them. Development Mode apps lost those batch endpoints in 2026: at the
  first refusal it switches to one call per item (`/tracks/{id}`,
  `/artists/{id}`) until the next restart. In Development Mode, a full history
  of a few thousand tracks is therefore looked up over hours, the most played
  first;
- an item Spotify does not know is not asked again; it keeps its names.

Until an item is looked up, it is shown with the names of the history file and
`null` ids and images (see the table above).
### Importing your history

Request your **Extended streaming history** from Spotify's privacy page. Send
the zip Spotify gives you as it is (512 MB max, 1 GB of history files once
uncompressed). The upload is written to an anonymous temporary file (in
`TMPDIR`, `/tmp` by default; deleted when the import ends) rather than held in
memory, so the server needs that much free disk space there. A body over its
limit gets `413`. A reverse proxy in front must let such bodies through (Caddy
does by default; nginx needs `client_max_body_size 512m;`):

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

The answer for a single file:

```json
{ "received": 15000, "imported": 14200, "skipped_short": 650, "skipped_not_track": 100, "skipped_duplicate": 50 }
```

For a zip, the same counts per file, their `totals`, and the other files:

```json
{
  "files": [
    { "name": "Spotify Extended Streaming History/Streaming_History_Audio_2021_0.json",
      "received": 15000, "imported": 14200, "skipped_short": 650, "skipped_not_track": 100, "skipped_duplicate": 50 }
  ],
  "totals": { "received": 15000, "imported": 14200, "skipped_short": 650, "skipped_not_track": 100, "skipped_duplicate": 50 },
  "ignored_files": ["Spotify Extended Streaming History/ReadMeFirst_ExtendedStreamingHistory.pdf"]
}
```

The history files only name the tracks: their artists' and albums' ids,
covers and genres come later, from the [metadata backfill](#spotify-metadata).
