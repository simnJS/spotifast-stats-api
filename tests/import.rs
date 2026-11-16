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
    post_raw(state, token, "application/json", body.into_bytes()).await
}

async fn post_zip(state: &AppState, token: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    post_raw(state, token, "application/zip", body).await
}

/// Build a zip archive in memory: `(path, content)` pairs, deflated.
fn make_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn json_bytes(v: &Value) -> Vec<u8> {
    v.to_string().into_bytes()
}

async fn count(s: &AppState, table: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&s.pool)
        .await
        .unwrap();
    n
}

async fn post_raw(
    state: &AppState,
    token: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/import")
        .header("content-type", content_type);
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

/// Import an entry for track "a" (200 s listened, ending 2021-03-01T10:00:00Z,
/// so started 09:56:40Z) while the user already has a poll play at
/// `played_at_ms`. Returns the response body and the resulting play count.
async fn import_a_with_existing_play(played_at_ms: i64) -> (Value, i64) {
    let s = state().await;
    let (user, token) = user_with_token(&s, "u1").await;
    let tid = db::upsert_track(&s.pool, "a", "Known", "KA", "KB", Some(200_000))
        .await
        .unwrap();
    db::insert_play(
        &s.pool,
        user,
        tid,
        played_at_ms,
        200_000,
        db::PlaySource::Poll,
    )
    .await
    .unwrap();
    let fixture = json!([track("2021-03-01T10:00:00Z", 200_000, "a")]);
    let (_, body) = post_import(&s, Some(&token), fixture.to_string()).await;
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    (body, n)
}

#[tokio::test]
async fn poll_play_stamped_at_stream_start_is_a_duplicate() {
    let (body, n) = import_a_with_existing_play(1_614_592_800_000 - 200_000 + 5_000).await;
    assert_eq!(body["imported"], 0);
    assert_eq!(body["skipped_duplicate"], 1);
    assert_eq!(n, 1);
}

#[tokio::test]
async fn poll_play_stamped_at_stream_end_is_a_duplicate() {
    let (body, n) = import_a_with_existing_play(1_614_592_800_000 + 30_000).await;
    assert_eq!(body["imported"], 0);
    assert_eq!(body["skipped_duplicate"], 1);
    assert_eq!(n, 1);
}

#[tokio::test]
async fn separate_earlier_listen_outside_window_is_imported() {
    // 5 minutes before the start of the stream.
    let (body, n) = import_a_with_existing_play(1_614_592_800_000 - 200_000 - 300_000).await;
    assert_eq!(body["imported"], 1);
    assert_eq!(body["skipped_duplicate"], 0);
    assert_eq!(n, 2);
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

// ---- zip archives -------------------------------------------------------

const DIR: &str = "Spotify Extended Streaming History/";

/// Two audio files in a folder, plus entries that must be ignored.
fn export_zip() -> Vec<u8> {
    let part0 = json!([
        track("2021-03-01T10:00:00Z", 200_000, "a"),
        track("2021-03-01T10:05:00Z", 180_000, "b"),
        track("2021-03-01T10:10:00Z", 5_000, "a"), // short
        {"ts": "2021-03-01T11:00:00Z", "ms_played": 900_000, "spotify_track_uri": null},
    ]);
    let part1 = json!([
        track("2021-03-02T10:00:00Z", 100_000, "c"),
        track("2021-03-02T10:05:00Z", 100_000, "a"),
    ]);
    make_zip(&[
        (
            format!("{DIR}Streaming_History_Audio_2021_1.json").as_str(),
            json_bytes(&part1),
        ),
        (
            format!("{DIR}Streaming_History_Audio_2020-2021_0.json").as_str(),
            json_bytes(&part0),
        ),
        (
            format!("{DIR}Streaming_History_Video_2021.json").as_str(),
            b"not even json".to_vec(),
        ),
        (
            format!("{DIR}ReadMeFirst_ExtendedStreamingHistory.pdf").as_str(),
            b"%PDF-1.4".to_vec(),
        ),
        ("Userdata.json", b"[]".to_vec()),
    ])
}

#[tokio::test]
async fn zip_import_reports_per_file_and_ignores_the_rest() {
    let s = state().await;
    let (user, token) = user_with_token(&s, "u1").await;
    let (status, body) = post_zip(&s, Some(&token), export_zip()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "files": [
                {"name": format!("{DIR}Streaming_History_Audio_2020-2021_0.json"),
                 "received": 4, "imported": 2, "skipped_short": 1,
                 "skipped_not_track": 1, "skipped_duplicate": 0},
                {"name": format!("{DIR}Streaming_History_Audio_2021_1.json"),
                 "received": 2, "imported": 2, "skipped_short": 0,
                 "skipped_not_track": 0, "skipped_duplicate": 0},
            ],
            "totals": {"received": 6, "imported": 4, "skipped_short": 1,
                       "skipped_not_track": 1, "skipped_duplicate": 0},
            "ignored_files": [
                format!("{DIR}ReadMeFirst_ExtendedStreamingHistory.pdf"),
                format!("{DIR}Streaming_History_Video_2021.json"),
                "Userdata.json",
            ],
        })
    );
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays WHERE user_id = ?1")
        .bind(user)
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(n, 4);
}

#[tokio::test]
async fn zip_import_matches_files_in_any_folder() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let zip = make_zip(&[
        (
            "Streaming_History_Audio_1.json",
            json_bytes(&json!([track("2021-03-01T10:00:00Z", 200_000, "a")])),
        ),
        (
            "deep/er/folder/Streaming_History_Audio_2.json",
            json_bytes(&json!([track("2021-03-01T12:00:00Z", 200_000, "b")])),
        ),
    ]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["imported"], 2);
    assert_eq!(body["files"].as_array().unwrap().len(), 2);
    assert_eq!(body["ignored_files"], json!([]));
}

#[tokio::test]
async fn zip_import_does_not_double_count_across_files() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    // The same listen appears in both files (the second one 20 s off).
    let zip = make_zip(&[
        (
            "Streaming_History_Audio_1.json",
            json_bytes(&json!([track("2021-03-01T10:00:00Z", 200_000, "a")])),
        ),
        (
            "Streaming_History_Audio_2.json",
            json_bytes(&json!([
                track("2021-03-01T10:00:20Z", 200_000, "a"),
                track("2021-03-01T12:00:00Z", 200_000, "b"),
            ])),
        ),
    ]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["files"][0]["imported"], 1);
    assert_eq!(body["files"][1]["imported"], 1);
    assert_eq!(body["files"][1]["skipped_duplicate"], 1);
    assert_eq!(body["totals"]["imported"], 2);
    assert_eq!(body["totals"]["skipped_duplicate"], 1);
    assert_eq!(count(&s, "plays").await, 2);
}

#[tokio::test]
async fn zip_reimport_is_idempotent() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let (_, first) = post_zip(&s, Some(&token), export_zip()).await;
    assert_eq!(first["totals"]["imported"], 4);
    let (status, second) = post_zip(&s, Some(&token), export_zip()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["totals"]["imported"], 0);
    assert_eq!(second["totals"]["skipped_duplicate"], 4);
    assert_eq!(count(&s, "plays").await, 4);
}

#[tokio::test]
async fn zip_with_invalid_inner_json_is_400_and_writes_nothing() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let zip = make_zip(&[
        (
            "Streaming_History_Audio_0.json",
            json_bytes(&json!([track("2021-03-01T10:00:00Z", 200_000, "a")])),
        ),
        ("Streaming_History_Audio_1.json", b"{ broken".to_vec()),
    ]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("Streaming_History_Audio_1.json"),
        "{message}"
    );
    assert_eq!(count(&s, "plays").await, 0);
    assert_eq!(count(&s, "tracks").await, 0);
}

#[tokio::test]
async fn zip_with_invalid_entry_is_400_and_writes_nothing() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let zip = make_zip(&[
        (
            "Streaming_History_Audio_0.json",
            json_bytes(&json!([track("2021-03-01T10:00:00Z", 200_000, "a")])),
        ),
        (
            "Streaming_History_Audio_1.json",
            json_bytes(&json!([
                {"ts": "yesterday", "ms_played": 1, "spotify_track_uri": "spotify:track:b"}
            ])),
        ),
    ]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("Streaming_History_Audio_1.json"),
        "{message}"
    );
    assert_eq!(count(&s, "plays").await, 0);
}

#[tokio::test]
async fn zip_without_matching_file_is_400() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let zip = make_zip(&[
        ("Streaming_History_Video_2021.json", b"[]".to_vec()),
        ("Userdata.json", b"[]".to_vec()),
        ("StreamingHistory0.json", b"[]".to_vec()),
    ]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(message.contains("Streaming_History_Audio_"), "{message}");
    assert_eq!(count(&s, "plays").await, 0);
}

#[tokio::test]
async fn corrupt_zip_is_400_and_writes_nothing() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let (status, body) = post_zip(&s, Some(&token), b"this is not a zip".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().is_some_and(|m| !m.is_empty()));
    // A truncated archive (central directory cut off) is corrupt too.
    let mut zip = export_zip();
    zip.truncate(zip.len() / 2);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(count(&s, "plays").await, 0);
}

#[tokio::test]
async fn zip_with_unsafe_path_is_400() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let zip = make_zip(&[(
        "../Streaming_History_Audio_0.json",
        json_bytes(&json!([track("2021-03-01T10:00:00Z", 200_000, "a")])),
    )]);
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(count(&s, "plays").await, 0);
}

#[tokio::test]
async fn zip_requires_authentication() {
    let s = state().await;
    let (status, _) = post_zip(&s, None, export_zip()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = post_zip(&s, Some("nope"), export_zip()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn large_zip_imports_quickly() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "u1").await;
    let names: Vec<String> = (0..10)
        .map(|f| format!("Streaming_History_Audio_{f:02}.json"))
        .collect();
    let files: Vec<(&str, Vec<u8>)> = (0..10i64)
        .map(|f| {
            let entries: Vec<Value> = (0..15_000i64)
                .map(|i| {
                    let secs = 1_600_000_000 + (f * 15_000 + i) * 300;
                    let ts = chrono::DateTime::from_timestamp(secs, 0)
                        .unwrap()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    track(&ts, 120_000, &format!("t{}", i % 500))
                })
                .collect();
            (
                names[f as usize].as_str(),
                json_bytes(&Value::Array(entries)),
            )
        })
        .collect();
    let zip = make_zip(&files);
    let started = std::time::Instant::now();
    let (status, body) = post_zip(&s, Some(&token), zip).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["received"], 150_000);
    assert_eq!(body["totals"]["imported"], 150_000);
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
}
