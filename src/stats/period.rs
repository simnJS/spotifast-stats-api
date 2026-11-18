//! Parsing of the query parameters shared by the stats endpoints.

use chrono::{DateTime, Months, Utc};

use crate::error::{AppError, AppResult};

/// A half-open period `[from, to)` in unix milliseconds (UTC).
#[derive(Debug, Clone, Copy)]
pub struct Period {
    pub from: i64,
    pub to: i64,
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

/// Parse a timestamp given as unix milliseconds or as RFC 3339.
pub fn parse_instant(name: &str, value: &str) -> AppResult<i64> {
    let v = value.trim();
    let digits = v.strip_prefix('-').unwrap_or(v);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return v
            .parse::<i64>()
            .map_err(|_| bad(format!("invalid `{name}`: out of range")));
    }
    DateTime::parse_from_rfc3339(v)
        .map(|d| d.timestamp_millis())
        .map_err(|_| bad(format!("invalid `{name}`: expected unix ms or RFC 3339")))
}

impl Period {
    /// Resolve the period from `range` or `from`/`to`, relative to `now_ms`.
    pub fn resolve(
        range: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        now_ms: i64,
    ) -> AppResult<Self> {
        if range.is_some() && (from.is_some() || to.is_some()) {
            return Err(bad("`range` cannot be combined with `from`/`to`"));
        }
        if from.is_some() || to.is_some() {
            let from = from.map(|v| parse_instant("from", v)).transpose()?;
            let to = to.map(|v| parse_instant("to", v)).transpose()?;
            let period = Self {
                from: from.unwrap_or(i64::MIN),
                to: to.unwrap_or(i64::MAX),
            };
            if period.from >= period.to {
                return Err(bad("`from` must be before `to`"));
            }
            return Ok(period);
        }
        let now = DateTime::<Utc>::from_timestamp_millis(now_ms)
            .ok_or_else(|| AppError::Internal("clock out of range".into()))?;
        let start = match range.unwrap_or("lifetime") {
            "lifetime" => None,
            "4w" => Some(now - chrono::Duration::weeks(4)),
            "6m" => now.checked_sub_months(Months::new(6)),
            "1y" => now.checked_sub_months(Months::new(12)),
            other => {
                return Err(bad(format!(
                    "invalid `range` `{other}`: expected 4w, 6m, 1y or lifetime"
                )));
            }
        };
        Ok(Self {
            from: start.map_or(i64::MIN, |d| d.timestamp_millis()),
            to: i64::MAX,
        })
    }

    /// The period of the same length that ends where this one starts, for
    /// rank changes. An open end stands for `now_ms`. `None` when this one has
    /// no start (lifetime) or is empty.
    pub fn previous(&self, now_ms: i64) -> Option<Self> {
        if self.from == i64::MIN {
            return None;
        }
        let end = if self.to == i64::MAX { now_ms } else { self.to };
        let length = end.checked_sub(self.from).filter(|l| *l > 0)?;
        Some(Self {
            from: self.from.checked_sub(length)?,
            to: self.from,
        })
    }
}

/// Parse the `limit` parameter (default 50, between 1 and 200).
pub fn parse_limit(value: Option<&str>) -> AppResult<i64> {
    match value {
        None => Ok(50),
        Some(v) => {
            let n: i64 = v
                .parse()
                .map_err(|_| bad("invalid `limit`: expected an integer"))?;
            if !(1..=200).contains(&n) {
                return Err(bad("invalid `limit`: must be between 1 and 200"));
            }
            Ok(n)
        }
    }
}

/// Parse the `offset` parameter (default 0).
pub fn parse_offset(value: Option<&str>) -> AppResult<i64> {
    match value {
        None => Ok(0),
        Some(v) => {
            let n: i64 = v
                .parse()
                .map_err(|_| bad("invalid `offset`: expected an integer"))?;
            if n < 0 {
                return Err(bad("invalid `offset`: must be >= 0"));
            }
            Ok(n)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2027-01-15T08:00:00Z.
    const NOW: i64 = 1_800_000_000_000;
    const DAY: i64 = 86_400_000;

    fn previous(range: Option<&str>, from: Option<&str>, to: Option<&str>) -> Option<(i64, i64)> {
        Period::resolve(range, from, to, NOW)
            .unwrap()
            .previous(NOW)
            .map(|p| (p.from, p.to))
    }

    #[test]
    fn previous_period_has_the_same_length_and_ends_where_it_starts() {
        assert_eq!(
            previous(Some("4w"), None, None),
            Some((NOW - 56 * DAY, NOW - 28 * DAY))
        );
        let six = Period::resolve(Some("6m"), None, None, NOW).unwrap();
        let before = six.previous(NOW).unwrap();
        assert_eq!(before.to, six.from);
        assert_eq!(six.from - before.from, NOW - six.from);
        assert_eq!(
            previous(None, Some("1000"), Some("3000")),
            Some((-1000, 1000))
        );
        // An open end stands for now.
        let from = (NOW - DAY).to_string();
        assert_eq!(
            previous(None, Some(&from), None),
            Some((NOW - 2 * DAY, NOW - DAY))
        );
    }

    #[test]
    fn lifetime_and_open_starts_have_no_previous_period() {
        assert_eq!(previous(None, None, None), None);
        assert_eq!(previous(Some("lifetime"), None, None), None);
        assert_eq!(previous(None, None, Some("1000")), None);
        // Starting after now with an open end: empty, nothing before.
        let later = (NOW + DAY).to_string();
        assert_eq!(previous(None, Some(&later), None), None);
    }
}
