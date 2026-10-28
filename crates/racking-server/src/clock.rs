//! Wall-clock helpers. Times are stored as Unix seconds and shown as RFC 3339 UTC,
//! which avoids a date crate for the one format the protocol uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The time source of presence (`GET /agents`), in Unix seconds. Injectable so
/// tests can move time past the online window instead of sleeping through it;
/// job timestamps always use the system clock.
#[derive(Clone)]
pub struct Clock(Option<Arc<AtomicI64>>);

impl Clock {
    pub fn system() -> Self {
        Self(None)
    }

    /// A clock that stands still at `start` until `advance` moves it; clones share it.
    pub fn manual(start: i64) -> Self {
        Self(Some(Arc::new(AtomicI64::new(start))))
    }

    /// Moves a manual clock forward; a system clock ignores it.
    pub fn advance(&self, secs: i64) {
        if let Some(t) = &self.0 {
            t.fetch_add(secs, Ordering::Relaxed);
        }
    }

    pub fn now(&self) -> i64 {
        match &self.0 {
            Some(t) => t.load(Ordering::Relaxed),
            None => now(),
        }
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`, using the days-to-civil
/// algorithm from Howard Hinnant's "chrono-Compatible Low-Level Date Algorithms".
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date, the inverse of the
/// algorithm in `rfc3339` (same source). Out-of-range months and days roll over,
/// so callers that need a real date check it by formatting back.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn formats_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_847_565), "2026-10-24T13:12:45Z");
    }
}
