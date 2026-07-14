// SPDX-License-Identifier: GPL-2.0

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::fmt;

use crate::mail::Mail;
use crate::parse;

/// A predicate over a parsed [`Mail`]. An inactive filter imposes no constraint
/// and matches every mail; an active one keeps only the mails it accepts.
pub trait Filter {
    fn is_active(&self) -> bool;
    fn matches(&self, mail: &Mail) -> bool;
}

/// Half-open date range `[start, end)` stored in UTC.
#[derive(Clone, Debug)]
pub struct DateRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

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

/// Case-insensitive substring constraint over one of the mail's text headers.
/// Subject and author differ only in which header they read.
#[derive(Clone, Debug)]
pub struct NameFilter {
    pub needle: Option<String>,
    field: fn(&Mail) -> &str,
}

impl NameFilter {
    /// Match against the mail's `Subject`.
    pub fn subject() -> Self {
        Self {
            needle: None,
            field: |mail| &mail.subject,
        }
    }

    /// Match against the whole decoded `From` header, so both the display name
    /// and the address are searchable.
    pub fn author() -> Self {
        Self {
            needle: None,
            field: |mail| &mail.from,
        }
    }

    /// Replace the needle from raw user text. Empty text clears the filter.
    pub fn set(&mut self, text: &str) {
        let trimmed = text.trim();
        self.needle = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
}

impl Filter for NameFilter {
    fn is_active(&self) -> bool {
        self.needle.is_some()
    }

    fn matches(&self, mail: &Mail) -> bool {
        match &self.needle {
            None => true,
            Some(n) => (self.field)(mail)
                .to_lowercase()
                .contains(&n.to_lowercase()),
        }
    }
}

impl fmt::Display for NameFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.needle {
            None => f.write_str("(none)"),
            Some(n) => f.write_str(n),
        }
    }
}

/// Date-range constraint. Empty (`None`) means "no constraint" — every mail
/// matches regardless of its `Date` header.
#[derive(Clone, Debug, Default)]
pub struct DateFilter {
    pub date_range: Option<DateRange>,
}

impl DateFilter {
    pub fn new() -> Self {
        Self { date_range: None }
    }

    /// Replace the range from raw user text. Accepts:
    ///   - `today`
    ///   - `yesterday`
    ///   - `YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM`
    ///
    /// Empty text clears the filter; malformed text returns an error and
    /// leaves the existing filter unchanged.
    pub fn set(&mut self, text: &str) -> Result<()> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.date_range = None;
            return Ok(());
        }
        let (start, end) = parse_date_range(trimmed)?;
        self.date_range = Some(DateRange { start, end });
        Ok(())
    }
}

fn local_midnight_to_utc(date: NaiveDate) -> Result<DateTime<Utc>> {
    let naive = date.and_hms_opt(0, 0, 0).context("date overflow")?;
    Local
        .from_local_datetime(&naive)
        .single()
        .context("ambiguous local time")
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_local_datetime(s: &str) -> Result<DateTime<Utc>> {
    let naive =
        NaiveDateTime::parse_from_str(s, "%Y/%m/%d %H:%M").context("expected YYYY/MM/DD HH:MM")?;
    Local
        .from_local_datetime(&naive)
        .single()
        .context("ambiguous local time")
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parse user-entered date filter text into a half-open UTC range. Accepts:
///   - `today`
///   - `yesterday`
///   - `YYYY/MM/DD HH:MM to YYYY/MM/DD HH:MM`
fn parse_date_range(trimmed: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let lower = trimmed.to_lowercase();
    if lower == "today" {
        let today = Local::now().date_naive();
        let tomorrow = today.succ_opt().context("date overflow")?;
        return Ok((
            local_midnight_to_utc(today)?,
            local_midnight_to_utc(tomorrow)?,
        ));
    }
    if lower == "yesterday" {
        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().context("date underflow")?;
        return Ok((
            local_midnight_to_utc(yesterday)?,
            local_midnight_to_utc(today)?,
        ));
    }
    let Some((start_s, end_s)) = trimmed.split_once(" to ") else {
        bail!("expected 'today', 'yesterday', or '<start> to <end>'");
    };
    let start = parse_local_datetime(start_s.trim()).context("parsing start date")?;
    let end = parse_local_datetime(end_s.trim()).context("parsing end date")?;
    Ok((start, end))
}

impl Filter for DateFilter {
    fn is_active(&self) -> bool {
        self.date_range.is_some()
    }

    fn matches(&self, mail: &Mail) -> bool {
        let Some(range) = &self.date_range else {
            return true;
        };
        let Some(date) = mail.date else {
            return false;
        };
        let utc = date.with_timezone(&Utc);
        utc >= range.start && utc < range.end
    }
}

impl fmt::Display for DateFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.date_range {
            None => f.write_str("(none)"),
            Some(r) => write!(f, "{}", r),
        }
    }
}

/// Message-ID constraint: matches the one mail whose `Message-ID` equals the
/// target (angle brackets and whitespace ignored). Empty means "no constraint".
#[derive(Clone, Debug, Default)]
pub struct MsgidFilter {
    /// Normalized target id (angle brackets and whitespace stripped).
    target: String,
}

impl MsgidFilter {
    pub fn new(message_id: &str) -> Self {
        Self {
            target: parse::normalize_message_id(message_id),
        }
    }
}

impl Filter for MsgidFilter {
    fn is_active(&self) -> bool {
        !self.target.is_empty()
    }

    fn matches(&self, mail: &Mail) -> bool {
        parse::normalize_message_id(&mail.message_id) == self.target
    }
}

impl fmt::Display for MsgidFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.target)
    }
}
