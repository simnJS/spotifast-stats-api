-- Adaptive poller. All *_ms columns are unix milliseconds (UTC).

-- Each user's listening pace, in seconds per play: measured on every page of
-- plays (time between its oldest and newest play / number of plays) and
-- smoothed with an exponential moving average. The next poll interval is
-- about 25 times this value. NULL = not measured yet.
ALTER TABLE users ADD COLUMN secs_per_play REAL;

-- App-wide Spotify rate limit. Spotify's quota belongs to the app, not to a
-- user: after a 429 on any call, nothing is sent to Spotify for anyone (poller,
-- /now-playing, token refresh, login) before rate_limited_until_ms. Single row.
CREATE TABLE app_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    -- End of the current (or last) block. NULL = never blocked.
    rate_limited_until_ms INTEGER,
    -- When the 429 that set it was received (exponential back-off without a
    -- usable Retry-After).
    rate_limited_at_ms INTEGER
);
-- The per-user deadline of 0002 (users.rate_limited_until_ms) is no longer
-- used: the latest one still stored becomes the app-wide block (it is ignored
-- once past), so a 429 received before the upgrade keeps holding.
INSERT INTO app_state (id, rate_limited_until_ms)
    SELECT 1, MAX(rate_limited_until_ms) FROM users;
UPDATE users SET rate_limited_until_ms = NULL;
