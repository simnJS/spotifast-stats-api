//! Spotify poller tests, against a wiremock server and an in-memory DB.
//! Time is injected (`NOW`), so deadlines and schedules are deterministic up to
//! the real duration of a poll (`SLACK_MS`) and the ±10 % jitter.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once};

use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::spotify::{poll_all_users, poll_user};
use spotifast_stats_api::{AppState, db};
use tracing_subscriber::EnvFilter;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Injected "now": 2027-01-15T08:00:00Z, in unix ms.
const NOW: i64 = 1_800_000_000_000;
/// Some cursor before `NOW` (newest `played_at` seen), in unix ms.
const CURSOR: i64 = 1_799_990_000_123;
/// Outcome times include the real duration of the poll (milliseconds against
/// wiremock): allow this much.
const SLACK_MS: i64 = 5_000;
/// `Config::for_tests()`: POLL_INTERVAL_SECS and POLL_MAX_INTERVAL_SECS.
const MIN_INTERVAL_S: i64 = 1200;
const MAX_INTERVAL_S: i64 = 2400;
const RECENT: &str = "/v1/me/player/recently-played";

async fn setup() -> (AppState, MockServer) {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    let state = AppState::new(db::test_pool().await, config);
    (state, server)
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn item(track_id: &str, played_at: &str, artists: &[&str]) -> Value {
    json!({
        "played_at": played_at,
        "track": {
            "id": track_id, "name": format!("Song {track_id}"), "duration_ms": 200_000,
            "artists": artists.iter().map(|a| json!({"name": a})).collect::<Vec<_>>(),
            "album": { "name": "Album" },
        }
    })
}

/// A user whose access token is valid for a day after `NOW`.
async fn fresh_user(state: &AppState, spotify_id: &str, access: &str) -> i64 {
    let id = db::insert_test_user(&state.pool, spotify_id).await.unwrap();
    sqlx::query("UPDATE users SET access_token = ?1, token_expires_at = ?2 WHERE id = ?3")
        .bind(access)
        .bind(NOW / 1000 + 86_400)
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    id
}

/// Set one integer column of a user row.
async fn set(state: &AppState, user_id: i64, column: &str, value: Option<i64>) {
    sqlx::query(&format!("UPDATE users SET {column} = ?1 WHERE id = ?2"))
        .bind(value)
        .bind(user_id)
        .execute(&state.pool)
        .await
        .unwrap();
}

async fn mock_recent(server: &MockServer, bearer: &str, items: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path(RECENT))
        .and(header("authorization", format!("Bearer {bearer}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": items })))
        .mount(server)
        .await;
}

async fn mock_token(server: &MockServer, access: &str, times: u64) {
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access, "token_type": "Bearer", "expires_in": 3600,
        })))
        .expect(times)
        .mount(server)
        .await;
}

async fn play_count(state: &AppState, user_id: i64) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays WHERE user_id = ?1")
        .bind(user_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    n
}

/// The poller columns of a user row.
#[derive(Debug, sqlx::FromRow)]
struct PollState {
    last_polled_at_ms: Option<i64>,
    next_poll_at_ms: Option<i64>,
    poll_interval_secs: Option<i64>,
    rate_limited_until_ms: Option<i64>,
    last_poll_ok_at_ms: Option<i64>,
    last_error: Option<String>,
    last_error_at_ms: Option<i64>,
    token_expires_at: i64,
}

async fn poll_state(state: &AppState, user_id: i64) -> PollState {
    sqlx::query_as(
        "SELECT last_polled_at_ms, next_poll_at_ms, poll_interval_secs, rate_limited_until_ms,
                last_poll_ok_at_ms, last_error, last_error_at_ms, token_expires_at
         FROM users WHERE id = ?1",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await
    .unwrap()
}

/// The interval is `interval_s` and the next poll `interval_s` after `from_ms`,
/// ±10 % jitter.
fn assert_scheduled(st: &PollState, from_ms: i64, interval_s: i64) {
    assert_eq!(st.poll_interval_secs, Some(interval_s), "{st:?}");
    let next = st.next_poll_at_ms.expect("next poll scheduled");
    let (lo, hi) = (
        from_ms + interval_s * 900,
        from_ms + interval_s * 1100 + SLACK_MS,
    );
    assert!(
        (lo..=hi).contains(&next),
        "next poll {next} not in [{lo}, {hi}]"
    );
}

/// (path, authorization header) of every request the server received so far.
async fn calls(server: &MockServer) -> Vec<(String, String)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let auth = r
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            (r.url.path().to_string(), auth.to_string())
        })
        .collect()
}

/// The smoothed pace stored for a user, in seconds per play.
async fn secs_per_play(state: &AppState, user_id: i64) -> Option<f64> {
    sqlx::query_scalar("SELECT secs_per_play FROM users WHERE id = ?1")
        .bind(user_id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

fn assert_pace(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("pace measured");
    assert!(
        (actual - expected).abs() < 0.01,
        "pace {actual} s per play, expected {expected}"
    );
}

/// Log lines of this crate written on the test's thread while a
/// [`Logs::capture`] guard lives.
///
/// One global subscriber, installed once, writes every event into the buffer of
/// the thread that emitted it when that thread is capturing (`#[tokio::test]`
/// runs the whole test on its own thread). A per-test `set_default` subscriber
/// would not do: tracing caches each call site's interest when a thread first
/// reaches it, so a call site first reached by another test, without a
/// subscriber, would stay disabled for this one too.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static CAPTURING: RefCell<Option<Logs>> = const { RefCell::new(None) };
}

/// Stops the capture of this thread when dropped.
struct Capture;

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURING.with(|c| *c.borrow_mut() = None);
    }
}

impl Logs {
    fn capture(&self) -> Capture {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new("spotifast_stats_api=debug"))
                .with_writer(|| ThreadWriter)
                .with_ansi(false)
                .try_init()
                .expect("no other global subscriber in this test binary");
        });
        // Call sites reached before the subscriber existed are enabled now.
        tracing::callsite::rebuild_interest_cache();
        CAPTURING.with(|c| *c.borrow_mut() = Some(self.clone()));
        Capture
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Appends to the buffer of the current thread's capture, if any.
struct ThreadWriter;

impl std::io::Write for ThreadWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURING.with(|c| {
            if let Some(logs) = c.borrow().as_ref() {
                logs.0.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Spotify's recently-played as documented: only the last 50 plays can be read,
/// `after` keeps those strictly after the cursor, newest first.
#[derive(Clone, Default)]
struct History(Arc<Mutex<Vec<i64>>>);

impl History {
    fn play(&self, played_at_ms: i64) {
        self.0.lock().unwrap().push(played_at_ms);
    }

    /// `count` plays `gap_ms` apart, the newest at `newest_ms`.
    fn plays(&self, newest_ms: i64, gap_ms: i64, count: i64) {
        for i in 0..count {
            self.play(newest_ms - i * gap_ms);
        }
    }
}

/// Every recently-played call is answered from a [`History`].
async fn mock_history(server: &MockServer) -> History {
    let history = History::default();
    Mock::given(method("GET"))
        .and(path(RECENT))
        .respond_with(history.clone())
        .mount(server)
        .await;
    history
}

impl Respond for History {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let param = |name: &str| {
            request
                .url
                .query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.parse::<i64>().unwrap())
        };
        let after = param("after");
        let limit = param("limit").unwrap_or(20) as usize;
        let mut plays = self.0.lock().unwrap().clone();
        plays.sort_unstable();
        plays.reverse();
        plays.truncate(50);
        let items: Vec<Value> = plays
            .into_iter()
            .filter(|&p| after.is_none_or(|a| p > a))
            .take(limit)
            .map(|p| item(&format!("t{p}"), &iso(p), &["A"]))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({ "items": items }))
    }
}

#[tokio::test]
async fn poll_inserts_plays_and_is_idempotent() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    mock_recent(
        &server,
        "tok",
        vec![
            item("t2", "2024-01-01T10:05:00.500Z", &["Second", "Feat"]),
            item("t1", "2024-01-01T10:00:00.000Z", &["First"]),
        ],
    )
    .await;

    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 2);
    // Same data again: nothing new.
    assert_eq!(poll_user(&state, user, NOW + 1_000).await.unwrap(), 0);
    assert_eq!(play_count(&state, user).await, 2);

    let row: (String, String, i64, i64, String) = sqlx::query_as(
        "SELECT t.artist_name, t.album_name, p.played_at, p.ms_played, p.source
         FROM plays p JOIN tracks t ON t.id = p.track_id WHERE t.spotify_id = 't2'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Second");
    assert_eq!(row.1, "Album");
    assert_eq!(row.2, 1_704_103_500_500);
    assert_eq!(row.3, 200_000);
    assert_eq!(row.4, "poll");

    let st = poll_state(&state, user).await;
    assert_eq!(st.last_polled_at_ms, Some(1_704_103_500_500));
    assert!(st.last_poll_ok_at_ms.unwrap() >= NOW + 1_000);
    assert_eq!(st.last_error, None);

    // The second request carried the cursor as `after`.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].url.query().unwrap().contains("limit=50"));
    assert!(!requests[0].url.query().unwrap().contains("after"));
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(k, v)| k == "after" && v == "1704103500500")
    );
}

#[tokio::test]
async fn after_parameter_is_the_stored_cursor() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;
    mock_recent(&server, "tok", vec![]).await;

    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 0);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let query: Vec<(String, String)> = requests[0]
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(query.contains(&("after".to_string(), CURSOR.to_string())));
    assert!(query.contains(&("limit".to_string(), "50".to_string())));
    // Nothing new: the cursor stays where it was.
    assert_eq!(
        poll_state(&state, user).await.last_polled_at_ms,
        Some(CURSOR)
    );
}

#[tokio::test]
async fn token_about_to_expire_is_refreshed_once_then_reused() {
    let (state, server) = setup().await;
    let user = db::insert_test_user(&state.pool, "u1").await.unwrap();
    // Expires in 30 s: within the 60 s margin.
    set(&state, user, "token_expires_at", Some(NOW / 1000 + 30)).await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "token_type": "Bearer", "expires_in": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "new-access",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 1);

    let (access, refresh, exp): (String, String, i64) =
        sqlx::query_as("SELECT access_token, refresh_token, token_expires_at FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(access, "new-access");
    assert_eq!(refresh, "refresh", "old refresh token is kept when omitted");
    assert_eq!(exp, NOW / 1000 + 3600);

    // Now valid: the next poll does not refresh again (expect(1) verified on drop).
    poll_user(&state, user, NOW + 600_000).await.unwrap();
}

#[tokio::test]
async fn one_recently_played_call_per_due_user_and_no_refresh_while_token_is_valid() {
    let (state, server) = setup().await;
    mock_token(&server, "unexpected", 0).await;
    let mut users = Vec::new();
    for name in ["a", "b", "c"] {
        let id = fresh_user(&state, name, &format!("tok-{name}")).await;
        Mock::given(method("GET"))
            .and(path(RECENT))
            .and(header(
                "authorization",
                format!("Bearer tok-{name}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
            .expect(1)
            .mount(&server)
            .await;
        users.push(id);
    }
    // Valid for 90 more seconds: beyond the 60 s margin, so not refreshed either.
    set(&state, users[2], "token_expires_at", Some(NOW / 1000 + 90)).await;

    assert_eq!(poll_all_users(&state, NOW).await, users);
    // No play: every one of them goes to the maximum interval.
    for &user in &users {
        assert_scheduled(&poll_state(&state, user).await, NOW, MAX_INTERVAL_S);
    }
    assert!(poll_all_users(&state, NOW + 60_000).await.is_empty());
    // expect(1) per user and expect(0) on /api/token are verified on drop.
}

#[tokio::test]
async fn two_users_keep_separate_plays() {
    let (state, server) = setup().await;
    let a = fresh_user(&state, "ua", "tok-a").await;
    let b = fresh_user(&state, "ub", "tok-b").await;
    mock_recent(
        &server,
        "tok-a",
        vec![item("shared", "2024-01-01T10:00:00Z", &["X"])],
    )
    .await;
    mock_recent(
        &server,
        "tok-b",
        vec![
            item("shared", "2024-01-01T10:00:00Z", &["X"]),
            item("only-b", "2024-01-01T11:00:00Z", &["Y"]),
        ],
    )
    .await;

    assert_eq!(poll_all_users(&state, NOW).await, [a, b]);

    assert_eq!(play_count(&state, a).await, 1);
    assert_eq!(play_count(&state, b).await, 2);
    let (tracks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tracks")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tracks, 2);
}

#[tokio::test]
async fn rate_limit_is_per_user_persisted_and_strictly_respected() {
    let (state, server) = setup().await;
    let bad = fresh_user(&state, "bad", "tok-bad").await;
    let good = fresh_user(&state, "good", "tok-good").await;
    Mock::given(method("GET"))
        .and(path(RECENT))
        .and(header("authorization", "Bearer tok-bad"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "27000"))
        .expect(1)
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "tok-good",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    assert_eq!(poll_all_users(&state, NOW).await, [bad, good]);
    assert_eq!(play_count(&state, good).await, 1);

    let st = poll_state(&state, bad).await;
    let until = st.rate_limited_until_ms.expect("deadline stored");
    let error_at = st.last_error_at_ms.expect("error time stored");
    assert_eq!(until - error_at, 27_000 * 1000, "now + Retry-After");
    assert!((NOW..NOW + SLACK_MS).contains(&error_at));
    assert!(st.last_error.unwrap().contains("429"));
    assert_eq!(st.last_polled_at_ms, None, "a failed poll keeps the cursor");
    assert_eq!(st.last_poll_ok_at_ms, None);
    assert_eq!(play_count(&state, bad).await, 0);
    assert_eq!(
        poll_state(&state, good).await.rate_limited_until_ms,
        None,
        "only that user is rate limited"
    );

    // Meanwhile the access token of `bad` expires: still no refresh before the deadline.
    set(&state, bad, "token_expires_at", Some(0)).await;
    // Restart: a new AppState on the same database only knows what the DB says.
    let state = AppState::new(state.pool.clone(), (*state.config).clone());
    let before = calls(&server).await.len();
    // `good` keeps being polled while `bad` waits.
    assert_eq!(poll_all_users(&state, NOW + 3_000_000).await, [good]);
    assert_eq!(poll_all_users(&state, until - 1).await, [good]);
    let err = poll_user(&state, bad, until - 1).await.unwrap_err();
    assert_eq!(err.kind(), "still_rate_limited");
    let seen = calls(&server).await;
    assert_eq!(seen.len(), before + 2, "only `good` was called");
    assert!(
        seen[before..]
            .iter()
            .all(|(p, auth)| p != "/api/token" && auth != "Bearer tok-bad")
    );

    // From the deadline on, `bad` is polled again: token refresh, then recently-played.
    mock_token(&server, "tok-bad-2", 1).await;
    mock_recent(
        &server,
        "tok-bad-2",
        vec![item("t2", "2024-01-01T11:00:00Z", &["B"])],
    )
    .await;
    assert!(poll_all_users(&state, until).await.contains(&bad));
    assert_eq!(play_count(&state, bad).await, 1);
    let st = poll_state(&state, bad).await;
    assert_eq!(st.rate_limited_until_ms, None);
    assert_eq!(st.last_error, None);
    assert_eq!(st.last_error_at_ms, None);
    assert!(st.last_poll_ok_at_ms.unwrap() >= until);
}

#[tokio::test]
async fn rate_limit_without_retry_after_backs_off_exponentially() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    Mock::given(method("GET"))
        .and(path(RECENT))
        .respond_with(ResponseTemplate::new(429))
        .expect(8)
        .mount(&server)
        .await;

    let mut t = NOW;
    for expected_s in [60, 120, 240, 480, 960, 1920, 3600, 3600] {
        let err = poll_user(&state, user, t).await.unwrap_err();
        assert_eq!(err.kind(), "rate_limited");
        assert_eq!(err.status(), Some(429));
        assert_eq!(err.retry_after(), None);
        let st = poll_state(&state, user).await;
        let until = st.rate_limited_until_ms.unwrap();
        assert_eq!(until - st.last_error_at_ms.unwrap(), expected_s * 1000);
        // Not a millisecond early.
        assert!(poll_all_users(&state, until - 1).await.is_empty());
        t = until;
    }
}

#[tokio::test]
async fn fast_listener_is_polled_at_the_minimum() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;
    let history = mock_history(&server).await;
    // 20 plays 40 s apart since the cursor: 19 × 40 s / 20 = 38 s per play,
    // and 25 plays at that pace (950 s) take less than the minimum.
    history.plays(NOW - 5_000, 40_000, 20);

    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 20);
    assert_pace(secs_per_play(&state, user).await, 38.0);
    assert_scheduled(&poll_state(&state, user).await, NOW, MIN_INTERVAL_S);

    // Still skipping through tracks: 25 more plays, 40 s apart.
    let due = poll_state(&state, user).await.next_poll_at_ms.unwrap();
    assert!(poll_all_users(&state, due - 1).await.is_empty());
    history.plays(due - 5_000, 40_000, 25);
    assert_eq!(poll_all_users(&state, due).await, [user]);
    assert_eq!(play_count(&state, user).await, 45);
    // 24 × 40 s / 25 = 38.4 s, smoothed with the previous 38 s.
    assert_pace(secs_per_play(&state, user).await, (38.0 + 38.4) / 2.0);
    assert_scheduled(&poll_state(&state, user).await, due, MIN_INTERVAL_S);
}

#[tokio::test]
async fn slower_listener_is_polled_less_often_with_a_smoothed_pace() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;
    let history = mock_history(&server).await;
    // 10 plays 80 s apart: 9 × 80 s / 10 = 72 s per play, so 25 plays take
    // 1800 s, between the bounds.
    history.plays(NOW - 5_000, 80_000, 10);

    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 10);
    assert_pace(secs_per_play(&state, user).await, 72.0);
    assert_scheduled(&poll_state(&state, user).await, NOW, 1800);
    let due = poll_state(&state, user).await.next_poll_at_ms.unwrap();
    assert!(poll_all_users(&state, due - 1).await.is_empty());

    // Slower still: 6 plays 200 s apart, 5 × 200 s / 6 = 166.7 s per play.
    // Smoothed: (72 + 166.7) / 2 = 119.3 s, and 25 × 119.3 s is beyond the
    // maximum.
    history.plays(due - 5_000, 200_000, 6);
    assert_eq!(poll_all_users(&state, due).await, [user]);
    let pace = (72.0 + 1000.0 / 6.0) / 2.0;
    assert_pace(secs_per_play(&state, user).await, pace);
    assert_scheduled(&poll_state(&state, user).await, due, MAX_INTERVAL_S);

    // A single play has no time span to measure: the pace is kept.
    let due = poll_state(&state, user).await.next_poll_at_ms.unwrap();
    history.play(due - 5_000);
    assert_eq!(poll_all_users(&state, due).await, [user]);
    assert_eq!(play_count(&state, user).await, 17);
    assert_pace(secs_per_play(&state, user).await, pace);
    assert_scheduled(&poll_state(&state, user).await, due, MAX_INTERVAL_S);
}

#[tokio::test]
async fn idle_listener_goes_to_the_maximum_at_once() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;
    let history = mock_history(&server).await;
    history.plays(NOW - 5_000, 40_000, 20);
    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 20);
    assert_scheduled(&poll_state(&state, user).await, NOW, MIN_INTERVAL_S);

    // Nothing new: straight to the maximum, and the pace is kept.
    for _ in 0..2 {
        let due = poll_state(&state, user).await.next_poll_at_ms.unwrap();
        assert!(poll_all_users(&state, due - 1).await.is_empty());
        assert_eq!(poll_all_users(&state, due).await, [user]);
        assert_scheduled(&poll_state(&state, user).await, due, MAX_INTERVAL_S);
        assert_pace(secs_per_play(&state, user).await, 38.0);
    }

    // Listening again, as fast as before: back to the minimum.
    let due = poll_state(&state, user).await.next_poll_at_ms.unwrap();
    history.plays(due - 5_000, 40_000, 20);
    assert_eq!(poll_all_users(&state, due).await, [user]);
    assert_eq!(play_count(&state, user).await, 40);
    assert_scheduled(&poll_state(&state, user).await, due, MIN_INTERVAL_S);
}

#[tokio::test]
async fn full_page_polls_again_at_minimum_and_cursor_skips_no_readable_play() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    let history = History::default();
    Mock::given(method("GET"))
        .and(path(RECENT))
        .respond_with(history.clone())
        .mount(&server)
        .await;
    let base = NOW - 86_400_000;
    let at = |i: i64| base + i * 180_000; // a play every 3 minutes

    // 10 plays: less than a page.
    for i in 0..10 {
        history.play(at(i));
    }
    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 10);

    // 70 more plays before the next poll: Spotify only keeps the last 50, so
    // the page is full (the 20 oldest are gone for good, nobody can read them).
    for i in 10..80 {
        history.play(at(i));
    }
    set(&state, user, "poll_interval_secs", Some(MAX_INTERVAL_S)).await;
    let t = NOW + 600_000;
    assert_eq!(poll_user(&state, user, t).await.unwrap(), 50);
    let st = poll_state(&state, user).await;
    assert_eq!(st.last_polled_at_ms, Some(at(79)), "cursor = newest play");
    // Back to the minimum at once, whatever the pace (3 min per play here).
    assert_scheduled(&st, t, MIN_INTERVAL_S);
    assert_pace(secs_per_play(&state, user).await, 49.0 * 180.0 / 50.0);

    // 30 more: all of them come back after the cursor.
    for i in 80..110 {
        history.play(at(i));
    }
    let t = t + 600_000;
    assert_eq!(poll_user(&state, user, t).await.unwrap(), 30);

    // Every play Spotify still exposed at each poll was recorded.
    let recorded: Vec<i64> =
        sqlx::query_scalar("SELECT played_at FROM plays WHERE user_id = ?1 ORDER BY played_at")
            .bind(user)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    let expected: Vec<i64> = (0..10).chain(30..110).map(at).collect();
    assert_eq!(recorded, expected);

    // Each request's `after` was the newest play seen before it.
    let afters: Vec<Option<String>> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "after")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        afters,
        [None, Some(at(9).to_string()), Some(at(79).to_string())]
    );
}

#[tokio::test]
async fn full_page_without_new_plays_still_polls_again_at_minimum() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    let logs = Logs::default();
    let _guard = logs.capture();
    // The same full page every time (e.g. plays already imported).
    let page: Vec<Value> = (0..50)
        .map(|i| item(&format!("t{i}"), &iso(NOW - (i + 1) * 60_000), &["A"]))
        .collect();
    mock_recent(&server, "tok", page).await;

    // The first poll's page is the backlog of the last 50 plays: no warning.
    assert_eq!(poll_user(&state, user, NOW).await.unwrap(), 50);
    assert!(!logs.text().contains("full page"), "{}", logs.text());

    set(&state, user, "poll_interval_secs", Some(MAX_INTERVAL_S)).await;
    let t = NOW + 600_000;
    assert_eq!(poll_user(&state, user, t).await.unwrap(), 0);
    assert_scheduled(&poll_state(&state, user).await, t, MIN_INTERVAL_S);
    let text = logs.text();
    assert!(
        text.lines()
            .any(|l| l.contains("WARN") && l.contains("full page") && l.contains("minimum")),
        "{text}"
    );
}

#[tokio::test]
async fn revoked_refresh_token_backs_off_that_user_until_they_log_in_again() {
    let (state, server) = setup().await;
    let bad = db::insert_test_user(&state.pool, "bad").await.unwrap(); // expired token
    let good = fresh_user(&state, "good", "tok-good").await;
    set(&state, bad, "last_polled_at_ms", Some(CURSOR)).await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error": "invalid_grant", "error_description": "Refresh token revoked"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(RECENT))
        .and(header("authorization", "Bearer access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .expect(0)
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "tok-good",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    assert_eq!(poll_all_users(&state, NOW).await, [bad, good]);
    assert_eq!(play_count(&state, good).await, 1);
    assert_eq!(play_count(&state, bad).await, 0);

    let st = poll_state(&state, bad).await;
    assert!(st.last_error.unwrap().contains("must log in again"));
    let error_at = st.last_error_at_ms.unwrap();
    assert_eq!(st.next_poll_at_ms, Some(error_at + 6 * 3_600_000));
    assert_eq!(st.rate_limited_until_ms, None);
    assert_eq!(st.last_polled_at_ms, Some(CURSOR), "cursor kept");

    // Backed off: no call for hours (expect(1) on /api/token verified on drop).
    assert_eq!(poll_all_users(&state, NOW + 5 * 3_600_000).await, [good]);

    // A new login (fresh tokens), well before the 6 h are over, lifts the
    // back-off at once.
    let relogin = NOW + 5 * 3_600_000 + 600_000;
    sqlx::query(
        "UPDATE users SET access_token = 'relogged', refresh_token = 'refresh-2',
                token_expires_at = ?1 WHERE id = ?2",
    )
    .bind(relogin / 1000 + 3600)
    .bind(bad)
    .execute(&state.pool)
    .await
    .unwrap();
    mock_recent(
        &server,
        "relogged",
        vec![item("t2", "2024-01-01T11:00:00Z", &["B"])],
    )
    .await;
    assert!(poll_all_users(&state, relogin).await.contains(&bad));
    assert_eq!(play_count(&state, bad).await, 1);
    assert_eq!(poll_state(&state, bad).await.last_error, None);
}

#[tokio::test]
async fn unauthorized_marks_the_token_expired_without_retrying() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "stale").await;
    Mock::given(method("GET"))
        .and(path(RECENT))
        .and(header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"status": 401, "message": "The access token expired"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    mock_token(&server, "renewed", 1).await;

    let err = poll_user(&state, user, NOW).await.unwrap_err();
    assert_eq!(err.kind(), "unauthorized");
    assert_eq!(err.status(), Some(401));
    // No refresh and no retry in the same poll.
    assert_eq!(calls(&server).await.len(), 1);
    let st = poll_state(&state, user).await;
    assert_eq!(st.token_expires_at, 0, "token marked expired");
    assert!(st.last_error.unwrap().contains("401"));

    // The next poll refreshes the token first.
    mock_recent(
        &server,
        "renewed",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;
    let due = st.next_poll_at_ms.unwrap();
    assert!(poll_all_users(&state, due - 1).await.is_empty());
    assert_eq!(poll_all_users(&state, due).await, [user]);
    assert_eq!(play_count(&state, user).await, 1);
    let seen = calls(&server).await;
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1].0, "/api/token");
    assert_eq!(seen[2].1, "Bearer renewed");
}

#[tokio::test]
async fn spotify_error_keeps_the_cursor_and_stores_a_short_message() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "secret-access-token").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;
    Mock::given(method("GET"))
        .and(path(RECENT))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": { "status": 503, "message": "x".repeat(1_000) },
            "debug": "SECRET-BODY",
        })))
        .mount(&server)
        .await;

    let err = poll_user(&state, user, NOW).await.unwrap_err();
    assert_eq!(err.kind(), "http_status");
    assert_eq!(err.status(), Some(503));

    let st = poll_state(&state, user).await;
    assert_eq!(st.last_polled_at_ms, Some(CURSOR), "cursor kept");
    assert_eq!(st.last_poll_ok_at_ms, None);
    assert!((NOW..NOW + SLACK_MS).contains(&st.last_error_at_ms.unwrap()));
    assert_eq!(play_count(&state, user).await, 0);
    let message = st.last_error.clone().unwrap();
    assert!(message.contains("503"), "{message}");
    assert!(message.contains(&"x".repeat(200)), "{message}");
    assert!(!message.contains(&"x".repeat(201)), "truncated: {message}");
    assert!(!message.contains("SECRET-BODY"));
    assert!(!message.contains("secret-access-token"));
    // Backed off like an idle poll.
    assert_scheduled(&st, NOW, 2 * MIN_INTERVAL_S);

    // The next successful poll clears the error.
    server.reset().await;
    mock_recent(
        &server,
        "secret-access-token",
        vec![item("t1", &iso(CURSOR + 1), &["A"])],
    )
    .await;
    let due = st.next_poll_at_ms.unwrap();
    assert_eq!(poll_all_users(&state, due).await, [user]);
    let st = poll_state(&state, user).await;
    assert_eq!(st.last_error, None);
    assert_eq!(st.last_error_at_ms, None);
    assert!(st.last_poll_ok_at_ms.unwrap() >= due);
    assert_eq!(st.last_polled_at_ms, Some(CURSOR + 1));
}

#[tokio::test]
async fn network_error_is_categorised_and_keeps_the_cursor() {
    // Config::for_tests() points at port 1, where nothing listens.
    let state = AppState::new(db::test_pool().await, Config::for_tests());
    let user = fresh_user(&state, "u1", "tok").await;
    set(&state, user, "last_polled_at_ms", Some(CURSOR)).await;

    let err = poll_user(&state, user, NOW).await.unwrap_err();
    assert_eq!(err.kind(), "network");
    assert_eq!(err.status(), None);

    let st = poll_state(&state, user).await;
    assert_eq!(st.last_polled_at_ms, Some(CURSOR));
    assert!(st.last_error.unwrap().contains("network error"));
    assert_eq!(st.last_poll_ok_at_ms, None);
}
