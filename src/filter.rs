// SPDX-License-Identifier: GPL-2.0

//! The one constraint both apps share: a date window over a mail's `Date:`.
//! Parsing user text (local wall-clock) into UTC and testing a mail against
//! the window live here; how a window is prompted for, displayed as "(none)",
//! or combined with other constraints is each app's own.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::fmt;

use crate::mail::Mail;

/// Half-open date range `[start, end)` stored in UTC.
#[derive(Clone, Debug)]
pub struct DateRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl DateRange {
    /// Parse user-entered text into a range. Times are read as local
    /// wall-clock and stored in UTC. Accepts:
    ///   - `today`
    ///   - `yesterday`
    ///   - `YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM`
    pub fn parse(text: &str) -> Result<DateRange> {
        let trimmed = text.trim();
        let lower = trimmed.to_lowercase();
        if lower == "today" {
            let today = Local::now().date_naive();
            let tomorrow = today.succ_opt().context("date overflow")?;
            return Ok(DateRange {
                start: local_midnight_to_utc(today)?,
                end: local_midnight_to_utc(tomorrow)?,
            });
        }
        if lower == "yesterday" {
            let today = Local::now().date_naive();
            let yesterday = today.pred_opt().context("date underflow")?;
            return Ok(DateRange {
                start: local_midnight_to_utc(yesterday)?,
                end: local_midnight_to_utc(today)?,
            });
        }
        let Some((start_s, end_s)) = trimmed.split_once(" to ") else {
            bail!("expected 'today', 'yesterday', or '<start> to <end>'");
        };
        Ok(DateRange {
            start: parse_local_datetime(start_s.trim()).context("parsing start date")?,
            end: parse_local_datetime(end_s.trim()).context("parsing end date")?,
        })
    }

    /// Whether the mail's `Date:` falls in the range. A mail with no parsable
    /// date is outside every range.
    pub fn contains(&self, mail: &Mail) -> bool {
        let Some(date) = mail.date else {
            return false;
        };
        let utc = date.with_timezone(&Utc);
        utc >= self.start && utc < self.end
    }
}

/// Rendered in local time, the way it was typed.
impl fmt::Display for DateRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} to {}",
            self.start.with_timezone(&Local).format("%Y/%m/%d %H:%M"),
            self.end.with_timezone(&Local).format("%Y/%m/%d %H:%M"),
        )
    }
}

/// Read a wall-clock time as local and convert it to UTC. Errors on the hour a
/// DST jump makes ambiguous or skips entirely.
fn local_to_utc(naive: NaiveDateTime) -> Result<DateTime<Utc>> {
    Local
        .from_local_datetime(&naive)
        .single()
        .context("ambiguous local time")
        .map(|dt| dt.with_timezone(&Utc))
}

fn local_midnight_to_utc(date: NaiveDate) -> Result<DateTime<Utc>> {
    local_to_utc(date.and_hms_opt(0, 0, 0).context("date overflow")?)
}

fn parse_local_datetime(s: &str) -> Result<DateTime<Utc>> {
    local_to_utc(
        NaiveDateTime::parse_from_str(s, "%Y/%m/%d %H:%M").context("expected YYYY/MM/DD HH:MM")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn range(start: &str, end: &str) -> DateRange {
        DateRange::parse(&format!("{start} to {end}")).unwrap()
    }

    #[test]
    fn explicit_range_is_half_open() {
        let r = range("2026/01/01 00:00", "2026/01/02 00:00");
        assert!(r.start < r.end);
        let at = |s: &str| Mail {
            date: Some(
                Local
                    .from_local_datetime(
                        &NaiveDateTime::parse_from_str(s, "%Y/%m/%d %H:%M").unwrap(),
                    )
                    .single()
                    .unwrap()
                    .with_timezone(&FixedOffset::east_opt(0).unwrap()),
            ),
            ..Mail::default()
        };
        assert!(r.contains(&at("2026/01/01 00:00")));
        assert!(r.contains(&at("2026/01/01 23:59")));
        assert!(!r.contains(&at("2026/01/02 00:00")));
        assert!(!r.contains(&Mail::default())); // no Date: header
    }

    #[test]
    fn today_and_yesterday_abut() {
        let today = DateRange::parse("today").unwrap();
        let yesterday = DateRange::parse("Yesterday").unwrap();
        assert_eq!(yesterday.end, today.start);
    }

    #[test]
    fn malformed_text_is_an_error() {
        assert!(DateRange::parse("last week").is_err());
        assert!(DateRange::parse("2026/01/01 00:00").is_err());
        assert!(DateRange::parse("").is_err());
    }
}
