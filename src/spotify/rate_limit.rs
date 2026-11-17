//! App-wide Spotify rate limit.
//!
//! Spotify's quota belongs to the app, not to a user: a 429 on any call (poller,
//! `/now-playing`, token refresh, login) blocks every Spotify call, for every
//! user, until the block ends. Its end is stored in the single `app_state` row
//! (`migrations/0003_adaptive_poller.sql`) and read before each call, so it
//! holds across restarts and between the poller and the HTTP handlers.

use std::time::Duration;

use sqlx::SqlitePool;

use crate::clock::fmt_ms;
use crate::error::AppError;

/// Block after a 429 without a usable `Retry-After`: this much first, then
/// doubled while the 429s follow one another...
const BACKOFF_MIN_MS: i64 = 60_000;
/// ...up to this much.
const BACKOFF_MAX_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// The end of the block (unix ms), when one is in force at `now_ms`.
pub async fn blocked_until(pool: &SqlitePool, now_ms: i64) -> Result<Option<i64>, sqlx::Error> {
    let until = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT rate_limited_until_ms FROM app_state WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(until.filter(|&until| until > now_ms))
}

/// Store a 429 received at `at_ms`; returns the end of the block.
///
/// The block lasts `Retry-After`, or without a usable one 60 s, doubled while
/// the 429s follow one another, up to 1 h (see [`backoff_ms`]). A stored block
/// is never shortened: a call sent before it started may still come back with a
/// 429. A storage failure is logged; the end is returned anyway.
pub async fn record_429(pool: &SqlitePool, retry_after: Option<Duration>, at_ms: i64) -> i64 {
    match store_429(pool, retry_after, at_ms).await {
        Ok(until) => until,
        Err(e) => {
            let wait_ms = retry_after.map_or(BACKOFF_MIN_MS, duration_ms);
            tracing::error!(error = %e, "Spotify rate limit: cannot store the app-wide block");
            at_ms.saturating_add(wait_ms)
        }
    }
}

async fn store_429(
    pool: &SqlitePool,
    retry_after: Option<Duration>,
    at_ms: i64,
) -> Result<i64, sqlx::Error> {
    let (previous_until, previous_at): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT rate_limited_until_ms, rate_limited_at_ms FROM app_state WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    let wait_ms = match retry_after {
        Some(d) => duration_ms(d),
        None => backoff_ms(previous_until, previous_at, at_ms),
    };
    let until = at_ms.saturating_add(wait_ms);
    // Only ever moves the end later, even when two 429s race.
    let extended = sqlx::query(
        "UPDATE app_state SET rate_limited_until_ms = ?1, rate_limited_at_ms = ?2
         WHERE id = 1 AND (rate_limited_until_ms IS NULL OR rate_limited_until_ms < ?1)",
    )
    .bind(until)
    .bind(at_ms)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if extended {
        tracing::warn!(
            until = %fmt_ms(until),
            retry_after_s = ?retry_after.map(|d| d.as_secs()),
            "Spotify rate limit (HTTP 429): no call to Spotify, for any user, before the end of \
             the block"
        );
        return Ok(until);
    }
    let stored = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT rate_limited_until_ms FROM app_state WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(stored.map_or(until, |s| s.max(until)))
}

/// How long to block after a 429 without a usable `Retry-After`, received at
/// `at_ms`, given the previous block (its end and when it started).
///
/// - during that block (a call sent before it started): until its end, so it
///   stands unchanged;
/// - less than its length after its end (the 429s follow one another): twice
///   its length, at most 1 h;
/// - otherwise: 60 s.
fn backoff_ms(previous_until: Option<i64>, previous_at: Option<i64>, at_ms: i64) -> i64 {
    match (previous_until, previous_at) {
        (Some(until), _) if at_ms < until => until - at_ms,
        (Some(until), Some(at)) if until > at && at_ms <= until.saturating_add(until - at) => {
            (until - at)
                .saturating_mul(2)
                .clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS)
        }
        _ => BACKOFF_MIN_MS,
    }
}

/// Whole seconds from `now_ms` to `until_ms`, rounded up, at least 1: the
/// `Retry-After` of an answer refused because of the block.
pub fn retry_after_secs(until_ms: i64, now_ms: i64) -> u64 {
    let secs = until_ms.saturating_sub(now_ms).saturating_add(999) / 1000;
    u64::try_from(secs).unwrap_or(1).max(1)
}

/// "Spotify limite l’app jusqu’à HH:MM UTC": the end of the block rounded up to
/// the minute (never earlier than it), with the date when it is a day or more
/// away.
pub fn blocked_message(until_ms: i64, now_ms: i64) -> String {
    let shown_ms = until_ms.saturating_add(59_999) / 60_000 * 60_000;
    let Some(shown) = chrono::DateTime::from_timestamp_millis(shown_ms) else {
        return "Spotify limite l’app pour le moment".to_string();
    };
    if shown_ms.saturating_sub(now_ms) < DAY_MS {
        format!("Spotify limite l’app jusqu’à {} UTC", shown.format("%H:%M"))
    } else {
        format!(
            "Spotify limite l’app jusqu’au {} UTC",
            shown.format("%Y-%m-%d %H:%M")
        )
    }
}

/// The error answered while the block holds: 503, `Retry-After` and
/// [`blocked_message`].
pub fn blocked_error(until_ms: i64, now_ms: i64) -> AppError {
    AppError::SpotifyRateLimited {
        message: blocked_message(until_ms, now_ms),
        retry_after_secs: retry_after_secs(until_ms, now_ms),
    }
}

fn duration_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2027-01-15T08:00:00Z.
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn backoff_starts_at_a_minute_doubles_while_429s_follow_and_caps_at_an_hour() {
        assert_eq!(backoff_ms(None, None, NOW), 60_000);
        // Just after a 60 s block: 120 s.
        assert_eq!(
            backoff_ms(Some(NOW - 1_000), Some(NOW - 61_000), NOW),
            120_000
        );
        // Just after a 40 min block: capped at 1 h.
        assert_eq!(
            backoff_ms(Some(NOW - 1_000), Some(NOW - 2_401_000), NOW),
            3_600_000
        );
        // Long after the previous block: start over.
        assert_eq!(
            backoff_ms(Some(NOW - 120_000), Some(NOW - 180_000), NOW),
            60_000
        );
        // The end predates the start: no usable previous block.
        assert_eq!(
            backoff_ms(Some(NOW - 2_000), Some(NOW - 1_000), NOW),
            60_000
        );
    }

    #[test]
    fn a_429_during_the_block_leaves_its_end_alone() {
        assert_eq!(
            backoff_ms(Some(NOW + 30_000), Some(NOW - 30_000), NOW),
            30_000
        );
    }

    #[test]
    fn retry_after_is_rounded_up_and_at_least_a_second() {
        assert_eq!(retry_after_secs(NOW + 120_000, NOW), 120);
        assert_eq!(retry_after_secs(NOW + 1_001, NOW), 2);
        assert_eq!(retry_after_secs(NOW, NOW), 1);
        assert_eq!(retry_after_secs(NOW - 5_000, NOW), 1);
    }

    #[test]
    fn message_shows_the_end_rounded_up_to_the_minute_in_utc() {
        assert_eq!(
            blocked_message(NOW + 630_000, NOW),
            "Spotify limite l’app jusqu’à 08:11 UTC"
        );
        assert_eq!(
            blocked_message(NOW + 600_000, NOW),
            "Spotify limite l’app jusqu’à 08:10 UTC"
        );
        // Past midnight, still within a day.
        assert_eq!(
            blocked_message(NOW + 17 * 3_600_000, NOW),
            "Spotify limite l’app jusqu’à 01:00 UTC"
        );
        assert_eq!(
            blocked_message(NOW + 2 * DAY_MS, NOW),
            "Spotify limite l’app jusqu’au 2027-01-17 08:00 UTC"
        );
    }

    #[tokio::test]
    async fn the_block_is_stored_never_shortened_and_read_back() {
        let pool = crate::db::test_pool().await;
        assert_eq!(blocked_until(&pool, NOW).await.unwrap(), None);

        let until = record_429(&pool, Some(Duration::from_secs(600)), NOW).await;
        assert_eq!(until, NOW + 600_000);
        assert_eq!(blocked_until(&pool, NOW).await.unwrap(), Some(until));
        assert_eq!(blocked_until(&pool, until - 1).await.unwrap(), Some(until));
        assert_eq!(blocked_until(&pool, until).await.unwrap(), None);

        // A shorter Retry-After arriving later does not shorten it.
        let again = record_429(&pool, Some(Duration::from_secs(10)), NOW + 1_000).await;
        assert_eq!(again, until);
        assert_eq!(blocked_until(&pool, NOW).await.unwrap(), Some(until));

        // Without Retry-After, right after its end: twice its length.
        let next = record_429(&pool, None, until + 1_000).await;
        assert_eq!(next, until + 1_000 + 1_200_000);
    }
}
