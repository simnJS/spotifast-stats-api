-- Adaptive poller. All *_ms columns are unix milliseconds (UTC).

-- Each user's listening pace, in seconds per play: measured on every page of
-- plays (time between its oldest and newest play / number of plays) and
-- smoothed with an exponential moving average. The next poll interval is
-- about 25 times this value. NULL = not measured yet.
ALTER TABLE users ADD COLUMN secs_per_play REAL;
