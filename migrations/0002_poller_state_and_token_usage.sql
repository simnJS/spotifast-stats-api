-- Poller state, persisted so that it survives a restart.
-- All *_ms columns are unix milliseconds (UTC).

-- Earliest time the next recently-played call may be made for this user
-- (adaptive interval + jitter). NULL = poll as soon as possible.
ALTER TABLE users ADD COLUMN next_poll_at_ms INTEGER;
-- Current adaptive poll interval in seconds. NULL = POLL_INTERVAL_SECS (the minimum).
ALTER TABLE users ADD COLUMN poll_interval_secs INTEGER;
-- Spotify answered 429: no call for this user before this time.
ALTER TABLE users ADD COLUMN rate_limited_until_ms INTEGER;
-- Time of the last poll that succeeded (with or without new plays).
-- Note: last_polled_at_ms (0001) is the recently-played cursor, i.e. the newest played_at seen.
ALTER TABLE users ADD COLUMN last_poll_ok_at_ms INTEGER;
-- Last poll or refresh failure: short human-readable cause, never a token or a response body.
ALTER TABLE users ADD COLUMN last_error TEXT;
ALTER TABLE users ADD COLUMN last_error_at_ms INTEGER;

-- Last time an API token authenticated a request (updated at most about once a minute).
ALTER TABLE api_tokens ADD COLUMN last_used_at_ms INTEGER;
