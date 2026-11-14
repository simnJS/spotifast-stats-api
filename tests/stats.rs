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
    Seeded { state, a, b }
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
