use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::new(db::test_pool().await, Config::for_tests())
}

async fn post_import(state: &AppState, token: Option<&str>, body: String) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/import")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app(state.clone())
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn user_with_token(s: &AppState, spotify_id: &str) -> (i64, String) {
    let id = db::insert_test_user(&s.pool, spotify_id).await.unwrap();
    let token = auth::issue_token(&s.pool, id).await.unwrap();
    (id, token)
}

fn track(ts: &str, ms: i64, id: &str) -> Value {
    json!({
        "ts": ts,
        "platform": "android",
        "ms_played": ms,
        "conn_country": "FR",
        "master_metadata_track_name": format!("Song {id}"),
        "master_metadata_album_artist_name": "Artist",
        "master_metadata_album_album_name": "Album",
        "spotify_track_uri": format!("spotify:track:{id}"),
        "episode_name": null,
        "spotify_episode_uri": null,
        "skipped": null,
        "some_future_field": {"x": 1}
    })
}

fn mixed_fixture() -> Value {
    json!([
        track("2021-03-01T10:00:00Z", 200_000, "a"),
        track("2021-03-01T10:05:00Z", 180_000, "b"),
        track("2021-03-01T10:10:00Z", 5_000, "a"), // short
        track("2021-03-01T10:11:00Z", 29_999, "c"), // short
        {
            "ts": "2021-03-01T11:00:00Z", "ms_played": 900_000,
            "master_metadata_track_name": null,
            "spotify_track_uri": null,
            "episode_name": "Pod", "spotify_episode_uri": "spotify:episode:e1"
        },
        {
            "ts": "2021-03-01T11:20:00Z", "ms_played": 100_000,
            "spotify_track_uri": "spotify:local:x:y:z:1"
        },
        track("2021-03-01T12:00:00Z", 30_000, "c"),
    ])
}

#[tokio::test]
async fn mixed_fixture_counts_and_rows() {
    let s = state().await;
    let (user, token) = user_with_token(&s, "u1").await;
    let (status, body) = post_import(&s, Some(&token), mixed_fixture().to_string()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"received": 7, "imported": 3, "skipped_short": 2,
               "skipped_not_track": 2, "skipped_duplicate": 0})
    );
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT played_at, ms_played, source FROM plays WHERE user_id = ?1 ORDER BY played_at",
    )
    .bind(user)
    .fetch_all(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (1_614_592_800_000, 200_000, "import".to_string()),
            (1_614_593_100_000, 180_000, "import".to_string()),
            (1_614_600_000_000, 30_000, "import".to_string()),
        ]
    );
    let (name, artist, album, dur): (String, String, String, Option<i64>) = sqlx::query_as(
        "SELECT name, artist_name, album_name, duration_ms FROM tracks WHERE spotify_id = 'a'",
    )
    .fetch_one(&s.pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), artist.as_str(), album.as_str(), dur),
        ("Song a", "Artist", "Album", None)
    );
}

#[tokio::test]
async fn importing_twice_is_idempotent() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let fixture = mixed_fixture().to_string();
    post_import(&s, Some(&token), fixture.clone()).await;
    let (status, body) = post_import(&s, Some(&token), fixture).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported"], 0);
    assert_eq!(body["skipped_duplicate"], 3);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(n, 3);
}

#[tokio::test]
async fn dedups_against_polled_play_and_keeps_known_metadata() {
    let s = state().await;
    let (user, token) = user_with_token(&s, "u1").await;
    let tid = db::upsert_track(&s.pool, "a", "Known", "KA", "KB", Some(215_000))
        .await
        .unwrap();
    // Polled 20 s after the export's `ts` (2021-03-01T10:00:00Z).
    db::insert_play(
        &s.pool,
        user,
        tid,
        1_614_592_800_000 + 20_000,
        215_000,
        db::PlaySource::Poll,
    )
    .await
    .unwrap();
    let fixture = json!([
        track("2021-03-01T10:00:00Z", 200_000, "a"), // duplicate of the poll
        track("2021-03-01T10:30:00Z", 200_000, "a"), // outside the window
        track("2021-03-01T10:00:00Z", 200_000, "b"), // other track
    ]);
    let (_, body) = post_import(&s, Some(&token), fixture.to_string()).await;
    assert_eq!(body["imported"], 2);
    assert_eq!(body["skipped_duplicate"], 1);
    let (name, artist, dur): (String, String, Option<i64>) =
        sqlx::query_as("SELECT name, artist_name, duration_ms FROM tracks WHERE id = ?1")
            .bind(tid)
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!(
        (name.as_str(), artist.as_str(), dur),
        ("Known", "KA", Some(215_000))
    );
}

#[tokio::test]
async fn null_names_do_not_erase_known_ones() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    db::upsert_track(&s.pool, "a", "Known", "KA", "KB", None)
        .await
        .unwrap();
    let fixture = json!([{
        "ts": "2021-03-01T10:00:00Z", "ms_played": 100_000,
        "master_metadata_track_name": null,
        "spotify_track_uri": "spotify:track:a"
    }]);
    let (_, body) = post_import(&s, Some(&token), fixture.to_string()).await;
    assert_eq!(body["imported"], 1);
    let (name, album): (String, String) =
        sqlx::query_as("SELECT name, album_name FROM tracks WHERE spotify_id = 'a'")
            .fetch_one(&s.pool)
            .await
            .unwrap();
    assert_eq!((name.as_str(), album.as_str()), ("Known", "KB"));
}

#[tokio::test]
async fn users_are_isolated() {
    let s = state().await;
    let (u1, t1) = user_with_token(&s, "u1").await;
    let (u2, t2) = user_with_token(&s, "u2").await;
    let fixture = mixed_fixture().to_string();
    let (_, b1) = post_import(&s, Some(&t1), fixture.clone()).await;
    let (_, b2) = post_import(&s, Some(&t2), fixture).await;
    assert_eq!(b1["imported"], 3);
    assert_eq!(b2["imported"], 3);
    for u in [u1, u2] {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays WHERE user_id = ?1")
            .bind(u)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 3);
    }
}

#[tokio::test]
async fn malformed_body_is_400_and_writes_nothing() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    for bad in [
        "not json".to_string(),
        r#"{"ts": "x"}"#.to_string(),
        // valid first entry, broken second one: nothing may be written
        json!([
            track("2021-03-01T10:00:00Z", 200_000, "a"),
            {"ts": "yesterday", "ms_played": 1, "spotify_track_uri": "spotify:track:b"}
        ])
        .to_string(),
        json!([track("2021-03-01T10:00:00Z", 200_000, "a"), {"ts": "2021-03-01T10:00:00Z"}])
            .to_string(),
    ] {
        let (status, body) = post_import(&s, Some(&token), bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().is_some_and(|m| !m.is_empty()));
    }
    for table in ["plays", "tracks"] {
        let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} must stay empty");
    }
}

#[tokio::test]
async fn requires_authentication() {
    let s = state().await;
    let (status, _) = post_import(&s, None, "[]".into()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = post_import(&s, Some("nope"), "[]".into()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn large_file_imports_quickly() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let entries: Vec<Value> = (0..15_000)
        .map(|i| {
            let ts = chrono::DateTime::from_timestamp(1_600_000_000 + i * 300, 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            track(&ts, 120_000, &format!("t{}", i % 500))
        })
        .collect();
    let started = std::time::Instant::now();
    let (status, body) = post_import(&s, Some(&token), Value::Array(entries).to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported"], 15_000);
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
}
