use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::db::{PlaySource, insert_play, insert_test_user, upsert_track};
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;

/// Monday 2024-01-01T00:00:00Z in unix ms.
const D: i64 = 1_704_067_200_000;
const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;

async fn state() -> AppState {
    AppState::new(db::test_pool().await, Config::for_tests())
}

async fn get(state: &AppState, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ok(state: &AppState, uri: &str, token: &str) -> Value {
    let (status, body) = get(state, uri, Some(token)).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

struct Seeded {
    state: AppState,
    ua: i64,
    ub: i64,
    a: String,
    b: String,
}

async fn play(s: &AppState, user: i64, track: i64, at: i64, ms: i64) {
    assert!(
        insert_play(&s.pool, user, track, at, ms, PlaySource::Poll)
            .await
            .unwrap()
    );
}

/// User A has 7 plays over 4 tracks; user B plays track 1 five times.
async fn seeded() -> Seeded {
    let state = state().await;
    let pool = &state.pool;
    let ua = insert_test_user(pool, "a").await.unwrap();
    let ub = insert_test_user(pool, "b").await.unwrap();
    let t1 = upsert_track(pool, "s1", "Song1", "ArtistX", "AlbumP", Some(1))
        .await
        .unwrap();
    let t2 = upsert_track(pool, "s2", "Song2", "ArtistX", "AlbumP", Some(1))
        .await
        .unwrap();
    let t3 = upsert_track(pool, "s3", "Song3", "ArtistY", "AlbumQ", Some(1))
        .await
        .unwrap();
    let t4 = upsert_track(pool, "s4", "Song4", "ArtistX", "AlbumR", Some(1))
        .await
        .unwrap();
    play(&state, ua, t1, D + HOUR, 100_000).await;
    play(&state, ua, t1, D + 2 * HOUR, 200_000).await;
    play(&state, ua, t2, D + DAY + HOUR, 300_000).await;
    play(&state, ua, t3, D + 8 * DAY, 50_000).await;
    play(&state, ua, t3, D + 8 * DAY + HOUR, 50_000).await;
    play(&state, ua, t3, D + 9 * DAY, 50_000).await;
    play(&state, ua, t4, D + 31 * DAY, 400_000).await;
    for i in 0..5 {
        play(&state, ub, t1, D + i * HOUR, 1_000).await;
    }
    let a = auth::issue_token(pool, ua).await.unwrap();
    let b = auth::issue_token(pool, ub).await.unwrap();
    Seeded {
        state,
        ua,
        ub,
        a,
        b,
    }
}

fn names(body: &Value, key: &str) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i[key].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn stats_summary_happy_path() {
    let s = seeded().await;
    let body = ok(&s.state, "/stats/summary", &s.a).await;
    assert_eq!(
        body,
        json!({
            "plays": 7,
            "ms_played": 1_150_000,
            "distinct_tracks": 4,
            "distinct_artists": 2,
            "distinct_albums": 3,
            "first_played_at": D + HOUR,
            "last_played_at": D + 31 * DAY,
        })
    );
}

#[tokio::test]
async fn stats_summary_period_filtering() {
    let s = seeded().await;
    let uri = format!("/stats/summary?from={}&to={}", D + DAY, D + 9 * DAY);
    let body = ok(&s.state, &uri, &s.a).await;
    assert_eq!(
        body,
        json!({
            "plays": 3,
            "ms_played": 400_000,
            "distinct_tracks": 2,
            "distinct_artists": 2,
            "distinct_albums": 2,
            "first_played_at": D + DAY + HOUR,
            "last_played_at": D + 8 * DAY + HOUR,
        })
    );
    // RFC 3339 gives the same window ("to" is exclusive).
    let rfc = "/stats/summary?from=2024-01-02T00:00:00Z&to=2024-01-10T00:00:00Z";
    assert_eq!(ok(&s.state, rfc, &s.a).await, body);
    // Empty period.
    let empty = ok(&s.state, "/stats/summary?from=0&to=1000", &s.a).await;
    assert_eq!(empty["plays"], 0);
    assert_eq!(empty["ms_played"], 0);
    assert_eq!(empty["first_played_at"], Value::Null);
}

#[tokio::test]
async fn stats_range_filters_relative_to_now() {
    let state = state().await;
    let u = insert_test_user(&state.pool, "c").await.unwrap();
    let t1 = upsert_track(&state.pool, "r1", "R1", "A", "B", None)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    play(&state, u, t1, now - HOUR, 1_000).await;
    play(&state, u, t1, now - 60 * DAY, 2_000).await;
    play(&state, u, t1, now - 400 * DAY, 4_000).await;
    let token = auth::issue_token(&state.pool, u).await.unwrap();
    for (range, plays, ms) in [
        ("4w", 1, 1_000),
        ("6m", 2, 3_000),
        ("1y", 2, 3_000),
        ("lifetime", 3, 7_000),
    ] {
        let body = ok(&state, &format!("/stats/summary?range={range}"), &token).await;
        assert_eq!(body["plays"], plays, "range {range}");
        assert_eq!(body["ms_played"], ms, "range {range}");
    }
    // Default is lifetime.
    assert_eq!(ok(&state, "/stats/summary", &token).await["plays"], 3);
}

#[tokio::test]
async fn stats_top_tracks_sorting_and_pagination() {
    let s = seeded().await;
    let by_plays = ok(&s.state, "/stats/top/tracks", &s.a).await;
    assert_eq!(
        by_plays["items"][0],
        json!({
            "spotify_id": "s3", "name": "Song3", "artist_name": "ArtistY",
            "album_name": "AlbumQ", "plays": 3, "ms_played": 150_000,
        })
    );
    assert_eq!(
        names(&by_plays, "name"),
        ["Song3", "Song1", "Song4", "Song2"]
    );
    let plays: Vec<i64> = by_plays["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["plays"].as_i64().unwrap())
        .collect();
    assert_eq!(plays, [3, 2, 1, 1]);

    let by_duration = ok(&s.state, "/stats/top/tracks?sort=duration", &s.a).await;
    assert_eq!(
        names(&by_duration, "name"),
        ["Song4", "Song1", "Song2", "Song3"]
    );
    assert_eq!(by_duration["items"][0]["ms_played"], 400_000);

    let page = ok(&s.state, "/stats/top/tracks?limit=2&offset=1", &s.a).await;
    assert_eq!(names(&page, "name"), ["Song1", "Song4"]);
    let beyond = ok(&s.state, "/stats/top/tracks?offset=10", &s.a).await;
    assert_eq!(beyond["items"], json!([]));

    let period = format!("/stats/top/tracks?from={}&to={}", D + DAY, D + 9 * DAY);
    let filtered = ok(&s.state, &period, &s.a).await;
    assert_eq!(names(&filtered, "name"), ["Song3", "Song2"]);
}

#[tokio::test]
async fn stats_top_artists_group_by_artist() {
    let s = seeded().await;
    let body = ok(&s.state, "/stats/top/artists", &s.a).await;
    assert_eq!(
        body["items"],
        json!([
            { "artist_name": "ArtistX", "plays": 4, "ms_played": 1_000_000 },
            { "artist_name": "ArtistY", "plays": 3, "ms_played": 150_000 },
        ])
    );
    let page = ok(&s.state, "/stats/top/artists?limit=1&offset=1", &s.a).await;
    assert_eq!(names(&page, "artist_name"), ["ArtistY"]);
}

#[tokio::test]
async fn stats_top_albums_group_by_album_and_artist() {
    let s = seeded().await;
    // Plays tie (3 vs 3): broken by ms_played.
    let by_plays = ok(&s.state, "/stats/top/albums", &s.a).await;
    assert_eq!(
        by_plays["items"],
        json!([
            { "album_name": "AlbumP", "artist_name": "ArtistX", "plays": 3, "ms_played": 600_000 },
            { "album_name": "AlbumQ", "artist_name": "ArtistY", "plays": 3, "ms_played": 150_000 },
            { "album_name": "AlbumR", "artist_name": "ArtistX", "plays": 1, "ms_played": 400_000 },
        ])
    );
    let by_duration = ok(&s.state, "/stats/top/albums?sort=duration", &s.a).await;
    assert_eq!(
        names(&by_duration, "album_name"),
        ["AlbumP", "AlbumR", "AlbumQ"]
    );
}

#[tokio::test]
async fn stats_same_album_name_by_different_artists_stays_separate() {
    let state = state().await;
    let u = insert_test_user(&state.pool, "d").await.unwrap();
    let t1 = upsert_track(&state.pool, "x1", "One", "Art1", "Greatest Hits", None)
        .await
        .unwrap();
    let t2 = upsert_track(&state.pool, "x2", "Two", "Art2", "Greatest Hits", None)
        .await
        .unwrap();
    play(&state, u, t1, 1_000, 10).await;
    play(&state, u, t2, 2_000, 10).await;
    let token = auth::issue_token(&state.pool, u).await.unwrap();
    let body = ok(&state, "/stats/top/albums", &token).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        ok(&state, "/stats/summary", &token).await["distinct_albums"],
        2
    );
}

#[tokio::test]
async fn stats_listening_time_buckets() {
    let s = seeded().await;
    let day = ok(&s.state, "/stats/listening-time?bucket=day", &s.a).await;
    assert_eq!(
        day["items"],
        json!([
            { "start": D, "plays": 2, "ms_played": 300_000 },
            { "start": D + DAY, "plays": 1, "ms_played": 300_000 },
            { "start": D + 8 * DAY, "plays": 2, "ms_played": 100_000 },
            { "start": D + 9 * DAY, "plays": 1, "ms_played": 50_000 },
            { "start": D + 31 * DAY, "plays": 1, "ms_played": 400_000 },
        ])
    );
    // Weeks start on Monday.
    let week = ok(&s.state, "/stats/listening-time?bucket=week", &s.a).await;
    assert_eq!(
        week["items"],
        json!([
            { "start": D, "plays": 3, "ms_played": 600_000 },
            { "start": D + 7 * DAY, "plays": 3, "ms_played": 150_000 },
            { "start": D + 28 * DAY, "plays": 1, "ms_played": 400_000 },
        ])
    );
    let month = ok(&s.state, "/stats/listening-time?bucket=month", &s.a).await;
    assert_eq!(
        month["items"],
        json!([
            { "start": D, "plays": 6, "ms_played": 750_000 },
            { "start": D + 31 * DAY, "plays": 1, "ms_played": 400_000 },
        ])
    );
    let uri = format!("/stats/listening-time?bucket=month&from={}", D + 31 * DAY);
    let filtered = ok(&s.state, &uri, &s.a).await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn stats_history_newest_first_with_cursor() {
    let s = seeded().await;
    let all = ok(&s.state, "/history", &s.a).await;
    assert_eq!(
        names(&all, "name"),
        [
            "Song4", "Song3", "Song3", "Song3", "Song2", "Song1", "Song1"
        ]
    );
    assert_eq!(
        all["items"][0],
        json!({
            "played_at": D + 31 * DAY, "ms_played": 400_000, "spotify_id": "s4",
            "name": "Song4", "artist_name": "ArtistX", "album_name": "AlbumR",
        })
    );
    assert_eq!(all["next_before"], Value::Null);

    let p1 = ok(&s.state, "/history?limit=3", &s.a).await;
    assert_eq!(names(&p1, "name"), ["Song4", "Song3", "Song3"]);
    assert_eq!(p1["next_before"], D + 8 * DAY + HOUR);
    let uri = format!("/history?limit=3&before={}", p1["next_before"]);
    let p2 = ok(&s.state, &uri, &s.a).await;
    assert_eq!(names(&p2, "name"), ["Song3", "Song2", "Song1"]);
    assert_eq!(p2["items"][0]["played_at"], D + 8 * DAY);
    assert_eq!(p2["next_before"], D + 2 * HOUR);
    let uri = format!("/history?limit=3&before={}", p2["next_before"]);
    let p3 = ok(&s.state, &uri, &s.a).await;
    assert_eq!(p3["items"].as_array().unwrap().len(), 1);
    assert_eq!(p3["items"][0]["played_at"], D + HOUR);
    assert_eq!(p3["next_before"], Value::Null);
}

#[tokio::test]
async fn stats_endpoints_require_authentication() {
    let s = seeded().await;
    for uri in [
        "/stats/summary",
        "/stats/top/tracks",
        "/stats/top/artists",
        "/stats/top/albums",
        "/stats/listening-time?bucket=day",
        "/history",
    ] {
        let (status, _) = get(&s.state, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        let (status, _) = get(&s.state, uri, Some("bogus")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} bogus");
    }
}

#[tokio::test]
async fn stats_users_never_see_each_others_plays() {
    let s = seeded().await;
    let summary = ok(&s.state, "/stats/summary", &s.b).await;
    assert_eq!(summary["plays"], 5);
    assert_eq!(summary["ms_played"], 5_000);
    assert_eq!(summary["distinct_tracks"], 1);
    let tracks = ok(&s.state, "/stats/top/tracks", &s.b).await;
    assert_eq!(tracks["items"].as_array().unwrap().len(), 1);
    assert_eq!(tracks["items"][0]["plays"], 5);
    let artists = ok(&s.state, "/stats/top/artists", &s.b).await;
    assert_eq!(names(&artists, "artist_name"), ["ArtistX"]);
    let albums = ok(&s.state, "/stats/top/albums", &s.b).await;
    assert_eq!(names(&albums, "album_name"), ["AlbumP"]);
    let time = ok(&s.state, "/stats/listening-time?bucket=month", &s.b).await;
    assert_eq!(
        time["items"],
        json!([{ "start": D, "plays": 5, "ms_played": 5_000 }])
    );
    let history = ok(&s.state, "/history", &s.b).await;
    assert_eq!(history["items"].as_array().unwrap().len(), 5);
    // And A is unaffected by B's plays.
    assert_eq!(ok(&s.state, "/stats/summary", &s.a).await["plays"], 7);
}

#[tokio::test]
async fn stats_invalid_parameters_give_400() {
    let s = seeded().await;
    for uri in [
        "/stats/summary?range=2w",
        "/stats/summary?range=4w&from=0",
        "/stats/summary?from=yesterday",
        "/stats/summary?to=nope",
        "/stats/summary?from=2000&to=1000",
        "/stats/top/tracks?range=bad",
        "/stats/top/tracks?sort=name",
        "/stats/top/artists?sort=; DROP TABLE plays",
        "/stats/top/albums?sort=",
        "/stats/top/tracks?limit=0",
        "/stats/top/tracks?limit=201",
        "/stats/top/tracks?limit=abc",
        "/stats/top/tracks?offset=-1",
        "/stats/listening-time",
        "/stats/listening-time?bucket=year",
        "/stats/listening-time?bucket=day&range=nope",
        "/history?limit=0",
        "/history?limit=201",
        "/history?before=later",
    ] {
        let (status, body) = get(&s.state, &uri.replace(' ', "%20"), Some(&s.a)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
    // Limit bounds are inclusive.
    ok(&s.state, "/stats/top/tracks?limit=200", &s.a).await;
    ok(&s.state, "/history?limit=1", &s.a).await;
}

fn ts(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .timestamp_millis()
}

/// Expected `hours` / `weekdays` array: `n` entries starting at `first`, zeros
/// everywhere except the given `(index, plays, ms_played)`.
fn buckets(key: &str, first: i64, n: i64, nonzero: &[(i64, i64, i64)]) -> Value {
    let items: Vec<Value> = (first..first + n)
        .map(|i| {
            let (plays, ms) = nonzero
                .iter()
                .find(|(k, _, _)| *k == i)
                .map_or((0, 0), |(_, p, m)| (*p, *m));
            json!({ key: i, "plays": plays, "ms_played": ms })
        })
        .collect();
    Value::Array(items)
}

async fn user_with_track(state: &AppState, spotify_id: &str) -> (i64, i64, String) {
    let u = insert_test_user(&state.pool, spotify_id).await.unwrap();
    let t = upsert_track(
        &state.pool,
        &format!("t-{spotify_id}"),
        "Song",
        "Artist",
        "Album",
        None,
    )
    .await
    .unwrap();
    let token = auth::issue_token(&state.pool, u).await.unwrap();
    (u, t, token)
}

#[tokio::test]
async fn stats_hours_utc_and_paris_across_dst() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "h").await;
    // Paris switches to summer time on 2024-03-31 at 01:00 UTC.
    play(&state, u, t, ts("2024-03-30T23:30:00Z"), 1_000).await; // Sat UTC, Sun 00:30 CET
    play(&state, u, t, ts("2024-03-31T00:30:00Z"), 2_000).await; // Sun 01:30 CET
    play(&state, u, t, ts("2024-03-31T01:00:00Z"), 3_000).await; // Sun 03:00 CEST
    play(&state, u, t, ts("2024-03-31T12:00:00Z"), 4_000).await; // Sun 14:00 CEST
    play(&state, u, t, ts("2024-01-01T12:00:00Z"), 5_000).await; // Mon 13:00 CET

    let utc = ok(&state, "/stats/hours", &token).await;
    assert_eq!(
        utc["hours"],
        buckets(
            "hour",
            0,
            24,
            &[(0, 1, 2_000), (1, 1, 3_000), (12, 2, 9_000), (23, 1, 1_000)]
        )
    );
    assert_eq!(
        utc["weekdays"],
        buckets(
            "weekday",
            1,
            7,
            &[(1, 1, 5_000), (6, 1, 1_000), (7, 3, 9_000)]
        )
    );
    // Explicit UTC is the default.
    assert_eq!(ok(&state, "/stats/hours?tz=UTC", &token).await, utc);

    let paris = ok(&state, "/stats/hours?tz=Europe/Paris", &token).await;
    assert_eq!(
        paris["hours"],
        buckets(
            "hour",
            0,
            24,
            &[
                (0, 1, 1_000),
                (1, 1, 2_000),
                (3, 1, 3_000),
                (13, 1, 5_000),
                (14, 1, 4_000)
            ]
        )
    );
    assert_eq!(
        paris["weekdays"],
        buckets("weekday", 1, 7, &[(1, 1, 5_000), (7, 4, 10_000)])
    );

    // Period filtering: `to` is exclusive, so the 01:00 UTC play is left out.
    let uri = "/stats/hours?tz=Europe/Paris&from=2024-03-31T00:00:00Z&to=2024-03-31T01:00:00Z";
    let narrow = ok(&state, uri, &token).await;
    assert_eq!(narrow["hours"], buckets("hour", 0, 24, &[(1, 1, 2_000)]));
    assert_eq!(
        narrow["weekdays"],
        buckets("weekday", 1, 7, &[(7, 1, 2_000)])
    );
}

#[tokio::test]
async fn stats_hours_repeated_hour_when_summer_time_ends() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "h2").await;
    // Paris goes back to winter time on 2024-10-27 at 01:00 UTC: 02:30 happens twice.
    play(&state, u, t, ts("2024-10-26T22:30:00Z"), 10).await; // Sat UTC, Sun 00:30 CEST
    play(&state, u, t, ts("2024-10-27T00:30:00Z"), 20).await; // Sun 02:30 CEST
    play(&state, u, t, ts("2024-10-27T01:30:00Z"), 40).await; // Sun 02:30 CET
    let paris = ok(&state, "/stats/hours?tz=Europe/Paris", &token).await;
    assert_eq!(
        paris["hours"],
        buckets("hour", 0, 24, &[(0, 1, 10), (2, 2, 60)])
    );
    assert_eq!(paris["weekdays"], buckets("weekday", 1, 7, &[(7, 3, 70)]));
}

#[tokio::test]
async fn stats_hours_empty_history_is_all_zeros() {
    let state = state().await;
    let (_, _, token) = user_with_track(&state, "h3").await;
    let body = ok(&state, "/stats/hours", &token).await;
    assert_eq!(body["hours"], buckets("hour", 0, 24, &[]));
    assert_eq!(body["weekdays"], buckets("weekday", 1, 7, &[]));
    assert_eq!(body["hours"].as_array().unwrap().len(), 24);
    assert_eq!(body["weekdays"].as_array().unwrap().len(), 7);
}

fn run(days: i64, start: &str, end: &str) -> Value {
    json!({ "days": days, "start": start, "end": end })
}

fn none_run() -> Value {
    json!({ "days": 0, "start": null, "end": null })
}

/// The date `k` days before today (UTC).
fn day(k: i64) -> chrono::NaiveDate {
    chrono::Utc::now().date_naive() - chrono::Duration::days(k)
}

/// Noon UTC, `k` days before today (UTC).
fn days_ago(k: i64) -> i64 {
    day(k)
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

#[tokio::test]
async fn stats_streaks_empty_history() {
    let state = state().await;
    let (_, _, token) = user_with_track(&state, "s0").await;
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(
        body,
        json!({ "current": none_run(), "longest": none_run(), "total_days_listened": 0 })
    );
}

#[tokio::test]
async fn stats_streaks_gap_and_longest_in_the_past() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "s1").await;
    // Jan 1, 2, 3 (two plays on the 2nd), gap, Jan 11, 12.
    for at in [
        D + HOUR,
        D + DAY + HOUR,
        D + DAY + 2 * HOUR,
        D + 2 * DAY + HOUR,
        D + 10 * DAY + HOUR,
        D + 11 * DAY + HOUR,
    ] {
        play(&state, u, t, at, 1).await;
    }
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(
        body,
        json!({
            "current": none_run(),
            "longest": run(3, "2024-01-01", "2024-01-03"),
            "total_days_listened": 5,
        })
    );
}

#[tokio::test]
async fn stats_streaks_current_ending_today() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "s2").await;
    // 3-day run (6, 5, 4 days ago), gap, then yesterday and today.
    for k in [6, 5, 4, 1, 0] {
        play(&state, u, t, days_ago(k), 1).await;
    }
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(
        body,
        json!({
            "current": run(2, &day(1).to_string(), &day(0).to_string()),
            "longest": run(3, &day(6).to_string(), &day(4).to_string()),
            "total_days_listened": 5,
        })
    );
}

#[tokio::test]
async fn stats_streaks_current_may_end_yesterday_but_not_before() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "s3").await;
    for k in [3, 2] {
        play(&state, u, t, days_ago(k), 1).await;
    }
    // Last play two days ago: the streak is over.
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(body["current"], none_run());
    assert_eq!(
        body["longest"],
        run(2, &day(3).to_string(), &day(2).to_string())
    );
    // A play yesterday extends the run and keeps it current.
    play(&state, u, t, days_ago(1), 1).await;
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(
        body["current"],
        run(3, &day(3).to_string(), &day(1).to_string())
    );
    assert_eq!(body["current"], body["longest"]);
    assert_eq!(body["total_days_listened"], 3);
}

#[tokio::test]
async fn stats_streaks_longest_tie_is_the_earliest() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "s4").await;
    for at in [
        D + HOUR,
        D + DAY + HOUR,
        D + 5 * DAY + HOUR,
        D + 6 * DAY + HOUR,
    ] {
        play(&state, u, t, at, 1).await;
    }
    let body = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(body["longest"], run(2, "2024-01-01", "2024-01-02"));
}

#[tokio::test]
async fn stats_streaks_use_the_requested_time_zone() {
    let state = state().await;
    let (u, t, token) = user_with_track(&state, "s5").await;
    play(&state, u, t, ts("2024-03-31T22:30:00Z"), 1).await; // Paris: Apr 1, 00:30
    play(&state, u, t, ts("2024-04-01T10:00:00Z"), 1).await; // Paris: Apr 1, 12:00
    let utc = ok(&state, "/stats/streaks", &token).await;
    assert_eq!(utc["total_days_listened"], 2);
    assert_eq!(utc["longest"], run(2, "2024-03-31", "2024-04-01"));
    let paris = ok(&state, "/stats/streaks?tz=Europe/Paris", &token).await;
    assert_eq!(paris["total_days_listened"], 1);
    assert_eq!(paris["longest"], run(1, "2024-04-01", "2024-04-01"));
}

#[tokio::test]
async fn stats_time_zone_errors_give_400() {
    let s = seeded().await;
    for uri in [
        "/stats/hours?tz=Mars/Olympus",
        "/stats/hours?tz=",
        "/stats/hours?tz=Europe/Paris&range=bad",
        "/stats/streaks?tz=Nope",
        "/stats/streaks?tz=",
    ] {
        let (status, body) = get(&s.state, uri, Some(&s.a)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

fn close(value: &Value, expected: f64) {
    let v = value.as_f64().unwrap();
    assert!((v - expected).abs() < 1e-9, "{v} != {expected}");
}

#[tokio::test]
async fn stats_compare_common_items_and_similarity() {
    let s = seeded().await;
    let uri = format!("/stats/compare?with={}", s.ub);
    let body = ok(&s.state, &uri, &s.a).await;
    assert_eq!(
        body["with"],
        json!({ "id": s.ub, "display_name": "User b" })
    );
    // ArtistX: A 4 plays, B 5. ArtistY only A. min sum 4, max sum 5 + 3.
    assert_eq!(
        body["common_artists"],
        json!([{ "artist_name": "ArtistX", "my_plays": 4, "their_plays": 5 }])
    );
    assert_eq!(
        body["common_tracks"],
        json!([{
            "spotify_id": "s1", "name": "Song1", "artist_name": "ArtistX",
            "album_name": "AlbumP", "my_plays": 2, "their_plays": 5,
        }])
    );
    close(&body["similarity"], 0.5);
    // Only the documented keys, nothing else of the other user.
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["common_artists", "common_tracks", "similarity", "with"]
    );

    // Seen from the other side the plays are swapped, the score is the same.
    let uri = format!("/stats/compare?with={}", s.ua);
    let back = ok(&s.state, &uri, &s.b).await;
    assert_eq!(
        back["common_artists"],
        json!([{ "artist_name": "ArtistX", "my_plays": 5, "their_plays": 4 }])
    );
    close(&back["similarity"], 0.5);

    // Period filter: from D to D+1h (inclusive) A has one ArtistX play, B two.
    let uri = format!("/stats/compare?with={}&from={D}&to={}", s.ub, D + HOUR + 1);
    let narrow = ok(&s.state, &uri, &s.a).await;
    assert_eq!(
        narrow["common_artists"],
        json!([{ "artist_name": "ArtistX", "my_plays": 1, "their_plays": 2 }])
    );
    close(&narrow["similarity"], 0.5);
    // Nothing from B after the first day: no common items, similarity 0.
    let uri = format!("/stats/compare?with={}&from={}", s.ub, D + DAY);
    let none = ok(&s.state, &uri, &s.a).await;
    assert_eq!(none["common_artists"], json!([]));
    assert_eq!(none["common_tracks"], json!([]));
    close(&none["similarity"], 0.0);
}

#[tokio::test]
async fn stats_compare_ranking_limit_and_bounds() {
    let state = state().await;
    let pool = &state.pool;
    let u1 = insert_test_user(pool, "c1").await.unwrap();
    let u2 = insert_test_user(pool, "c2").await.unwrap();
    let mut tracks = Vec::new();
    for (id, name, artist) in [
        ("x1", "Song1", "A1"),
        ("x2", "Song2", "A2"),
        ("x3", "Song3", "A3"),
        ("x4", "Song4", "A3"),
        ("x5", "Song5", "O1"),
        ("x6", "Song6", "O2"),
    ] {
        tracks.push(
            upsert_track(pool, id, name, artist, "Al", None)
                .await
                .unwrap(),
        );
    }
    let &[x1, x2, x3, x4, x5, x6] = &tracks[..] else {
        unreachable!()
    };
    // (user, track, plays)
    let mut at = D;
    for (u, t, n) in [
        (u1, x1, 5),
        (u1, x2, 3),
        (u1, x3, 2),
        (u1, x4, 1),
        (u1, x5, 4),
        (u2, x1, 2),
        (u2, x2, 6),
        (u2, x3, 2),
        (u2, x6, 7),
    ] {
        for _ in 0..n {
            at += HOUR;
            play(&state, u, t, at, 1).await;
        }
    }
    let token = auth::issue_token(pool, u1).await.unwrap();
    let body = ok(&state, &format!("/stats/compare?with={u2}"), &token).await;
    // Ranked by the smaller count (3, 2, 2), ties by the sum (7 before 5).
    assert_eq!(
        body["common_artists"],
        json!([
            { "artist_name": "A2", "my_plays": 3, "their_plays": 6 },
            { "artist_name": "A1", "my_plays": 5, "their_plays": 2 },
            { "artist_name": "A3", "my_plays": 3, "their_plays": 2 },
        ])
    );
    assert_eq!(
        names(&json!({ "items": body["common_tracks"] }), "name"),
        ["Song2", "Song1", "Song3"]
    );
    assert_eq!(body["common_tracks"][0]["my_plays"], 3);
    assert_eq!(body["common_tracks"][0]["their_plays"], 6);
    // min: 2 + 3 + 2 = 7; max: 5 + 6 + 3 + 4 + 7 = 25.
    close(&body["similarity"], 7.0 / 25.0);

    // `limit` only trims the lists, not the score.
    let one = ok(&state, &format!("/stats/compare?with={u2}&limit=1"), &token).await;
    assert_eq!(one["common_artists"].as_array().unwrap().len(), 1);
    assert_eq!(one["common_tracks"].as_array().unwrap().len(), 1);
    assert_eq!(one["common_artists"][0]["artist_name"], "A2");
    close(&one["similarity"], 7.0 / 25.0);
    ok(
        &state,
        &format!("/stats/compare?with={u2}&limit=100"),
        &token,
    )
    .await;

    // Identical listening: score 1. No listening in common: score 0.
    let u3 = insert_test_user(pool, "c3").await.unwrap();
    let u4 = insert_test_user(pool, "c4").await.unwrap();
    play(&state, u3, x1, D, 1).await;
    play(&state, u4, x1, D + 1, 1).await;
    let t3 = auth::issue_token(pool, u3).await.unwrap();
    let same = ok(&state, &format!("/stats/compare?with={u4}"), &t3).await;
    close(&same["similarity"], 1.0);
    let u5 = insert_test_user(pool, "c5").await.unwrap();
    play(&state, u5, x6, D, 1).await;
    let disjoint = ok(&state, &format!("/stats/compare?with={u5}"), &t3).await;
    close(&disjoint["similarity"], 0.0);
    assert_eq!(disjoint["common_artists"], json!([]));
    // A user without any play at all.
    let u6 = insert_test_user(pool, "c6").await.unwrap();
    let empty = ok(&state, &format!("/stats/compare?with={u6}"), &t3).await;
    close(&empty["similarity"], 0.0);
}

#[tokio::test]
async fn stats_compare_errors() {
    let s = seeded().await;
    let (status, body) = get(&s.state, "/stats/compare?with=9999", Some(&s.a)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
    for uri in [
        format!("/stats/compare?with={}", s.ua),
        "/stats/compare".to_string(),
        "/stats/compare?with=abc".to_string(),
        format!("/stats/compare?with={}&limit=0", s.ub),
        format!("/stats/compare?with={}&limit=101", s.ub),
        format!("/stats/compare?with={}&limit=x", s.ub),
        format!("/stats/compare?with={}&range=bad", s.ub),
    ] {
        let (status, body) = get(&s.state, &uri, Some(&s.a)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn stats_new_endpoints_require_authentication() {
    let s = seeded().await;
    let compare = format!("/stats/compare?with={}", s.ub);
    for uri in ["/stats/hours", "/stats/streaks", compare.as_str()] {
        let (status, _) = get(&s.state, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        let (status, _) = get(&s.state, uri, Some("bogus")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} bogus");
    }
}
