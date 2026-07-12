// SPDX-License-Identifier: GPL-2.0

//! A parsed mail and everything needed to make sense of its raw bytes: header
//! extraction, MIME/encoding decoding and `Date` parsing all live here.

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local};

use crate::archive;

/// A mail: its raw RFC 5322 text plus the headers we care about, decoded, and
/// the mirror `epoch`/`commit` it was read from.
#[derive(Debug, Clone, Default)]
pub struct Mail {
    /// The raw mail text, kept so the body can be rendered on demand.
    pub raw: String,
    pub subject: String,
    pub from: String,
    pub author: String,
    pub sender: String,
    pub to: String,
    pub date: Option<DateTime<FixedOffset>>,
    pub message_id: String,
    /// The mail this one replies to: `In-Reply-To`, or the last id in
    /// `References` when `In-Reply-To` is absent. Empty for thread roots.
    pub in_reply_to: String,
    /// The whole `References` chain, oldest first. Its head names the thread
    /// root — the cover letter of a patch series.
    pub references: Vec<String>,
    /// `(n, m)` of a `[PATCH ... n/m ...]` subject: a cover letter is `0/m`, a
    /// lone `[PATCH]` is `1/1`. `None` when this is not a patch mail at all — a
    /// review reply, or ordinary discussion.
    pub patch_nums: Option<(u32, u32)>,
    pub epoch: u32,
    pub commit: String,
}

/// Fetch and parse the mail at `commit` from the local mirror of `list`'s
/// `epoch`.
pub fn fetch(list: &str, epoch: u32, commit: &str) -> Result<Mail> {
    let raw = archive::show_mail(list, epoch, commit)?;
    Ok(Mail::new(raw, epoch, commit.to_string()))
}

impl Mail {
    /// Parse raw mail text into a [`Mail`], tagged with the mirror `epoch`/`commit`
    /// it came from. Subject/From/To are MIME-decoded; the Message-ID is kept
    /// verbatim.
    pub fn new(raw: String, epoch: u32, commit: String) -> Self {
        let (headers, _body) = split_headers(&raw);
        let subject = parse_header(headers, "Subject")
            .map(|s| decode_mime_header(&s))
            .unwrap_or_default();
        let from = parse_header(headers, "From")
            .map(|s| decode_mime_header(&s))
            .unwrap_or_default();
        let to = parse_header(headers, "To")
            .map(|s| decode_mime_header(&s))
            .unwrap_or_default();
        let message_id = parse_header(headers, "Message-ID").unwrap_or_default();
        let references: Vec<String> = parse_header(headers, "References")
            .map(|r| r.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        // Prefer In-Reply-To; fall back to the last id in References.
        let in_reply_to = parse_header(headers, "In-Reply-To")
            .or_else(|| references.last().cloned())
            .unwrap_or_default();
        let date = parse_header(headers, "Date").and_then(|s| parse_rfc_date(&s));
        let author = pretty_from(&from);
        let sender = parse_sender(&from);
        let patch_nums = parse_patch_nums(&subject);
        Mail {
            raw,
            subject,
            from,
            author,
            sender,
            to,
            date,
            message_id,
            in_reply_to,
            references,
            patch_nums,
            epoch,
            commit,
        }
    }

    /// The `Date` header formatted for display in the local timezone, or `-`
    /// when unparseable/absent. Rendering every mail in one zone (rather than its
    /// own offset) makes a date-sorted list read monotonically.
    pub fn date_str(&self) -> String {
        match &self.date {
            Some(d) => d.with_timezone(&Local).format("%Y/%m/%d %H:%M").to_string(),
            None => "-".to_string(),
        }
    }

    /// Render the mail as plain text: a fixed set of headers followed by the
    /// decoded body. Used by the reader's detail view and the digest full format.
    pub fn render_full(&self) -> String {
        let (headers, _) = split_headers(&self.raw);

        let mut out = String::new();
        for field in ["From", "Date", "Subject", "To", "Cc", "Message-ID"] {
            if let Some(v) = parse_header(headers, field) {
                out.push_str(field);
                out.push_str(": ");
                out.push_str(&decode_mime_header(&v));
                out.push('\n');
            }
        }
        out.push_str("\n--\n\n");
        out.push_str(&self.body());
        out
    }

    /// The body, decoded according to `Content-Transfer-Encoding`.
    fn body(&self) -> String {
        let (headers, body) = split_headers(&self.raw);
        let encoding = parse_header(headers, "Content-Transfer-Encoding")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if encoding.trim() == "quoted-printable" {
            decode_quoted_printable(body)
        } else {
            body.to_string()
        }
    }

    /// An mbox draft of a reply to this mail: the sender becomes `To`, the
    /// original `To`/`Cc` become `Cc`, `In-Reply-To` threads it, and the body is
    /// quoted under an attribution line.
    ///
    /// `From` and `Message-ID` are deliberately absent: whoever sends the draft
    /// (`git send-email`) fills them in from the user's own identity, so no
    /// account information has to pass through here.
    pub fn reply_draft(&self) -> String {
        let subject = if self.subject.to_ascii_lowercase().starts_with("re:") {
            self.subject.clone()
        } else {
            format!("Re: {}", self.subject)
        };
        let (headers, _) = split_headers(&self.raw);
        let orig_cc = parse_header(headers, "Cc")
            .map(|s| decode_mime_header(&s))
            .unwrap_or_default();
        let cc = [self.to.as_str(), orig_cc.as_str()]
            .iter()
            .filter(|s| !s.trim().is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");

        let mut out = format!("Subject: {subject}\nTo: {}\n", self.from);
        if !cc.is_empty() {
            out.push_str(&format!("Cc: {cc}\n"));
        }
        if !self.message_id.is_empty() {
            out.push_str(&format!("In-Reply-To: {}\n", self.message_id));
        }
        out.push_str(&format!(
            "\nOn {}, {} wrote:\n\n",
            self.date_str(),
            self.author
        ));
        for line in self.body().lines() {
            out.push_str(if line.is_empty() { ">" } else { "> " });
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// The bare address out of a `Name <addr@host>` From header, falling back to the
/// whole header when it carries no angle brackets.
fn parse_sender(from: &str) -> String {
    match (from.find('<'), from.find('>')) {
        (Some(a), Some(b)) if a < b => from[a + 1..b].to_string(),
        _ => from.trim().to_string(),
    }
}

/// `(n, m)` out of a `[PATCH ... n/m ...]` subject; see [`Mail::patch_nums`].
fn parse_patch_nums(subject: &str) -> Option<(u32, u32)> {
    let subject = subject.trim();
    if subject.to_ascii_lowercase().starts_with("re:") {
        return None;
    }
    let tag = subject.strip_prefix('[')?.split_once(']')?.0;
    if !tag.to_ascii_uppercase().contains("PATCH") {
        return None;
    }
    // Both sides have to be numbers: a tag like "[PATCH net/tcp]" carries a
    // slash but no series numbering.
    let nums = tag.split_whitespace().find_map(|w| {
        let (n, m) = w.split_once('/')?;
        Some((n.parse::<u32>().ok()?, m.parse::<u32>().ok()?))
    });
    Some(nums.unwrap_or((1, 1)))
}

/// A display name from a `From` header: the quoted/plain name if present,
/// otherwise the bare address, otherwise the trimmed input.
fn pretty_from(from: &str) -> String {
    if let Some(start) = from.find('<') {
        let name = from[..start].trim().trim_matches('"');
        if !name.is_empty() {
            return name.to_string();
        }
        if let Some(end) = from[start..].find('>') {
            return from[start + 1..start + end].to_string();
        }
    }
    from.trim().to_string()
}

/// Split a raw mail into `(headers, body)` at the first blank line. A mail with
/// no blank-line separator is treated as all headers and an empty body.
fn split_headers(raw: &str) -> (&str, &str) {
    if let Some(i) = raw.find("\r\n\r\n") {
        (&raw[..i], &raw[i + 4..])
    } else if let Some(i) = raw.find("\n\n") {
        (&raw[..i], &raw[i + 2..])
    } else {
        (raw, "")
    }
}

/// Read a single header value, unfolding RFC 5322 continuation lines. The lookup
/// is case-insensitive; returns the raw (still MIME-encoded) value.
fn parse_header(headers: &str, name: &str) -> Option<String> {
    let target = name.to_lowercase();
    let mut iter = headers.lines().peekable();
    while let Some(line) = iter.next() {
        let Some(idx) = line.find(':') else { continue };
        if line[..idx].to_lowercase() != target {
            continue;
        }
        let mut v = line[idx + 1..].trim().to_string();
        while let Some(cont) = iter.peek() {
            if cont.starts_with(' ') || cont.starts_with('\t') {
                v.push(' ');
                v.push_str(cont.trim());
                iter.next();
            } else {
                break;
            }
        }
        return Some(v);
    }
    None
}

/// Parse a `Date` header (RFC 2822, falling back to RFC 3339).
fn parse_rfc_date(s: &str) -> Option<DateTime<FixedOffset>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc2822(s)
        .ok()
        .or_else(|| DateTime::parse_from_rfc3339(s).ok())
}

/// Decode an RFC 2045 quoted-printable body: join soft line breaks
/// (`=` followed by CRLF or LF) and decode `=XX` hex escapes.
fn decode_quoted_printable(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'=' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            i += 3;
            continue;
        }
        if i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push(((a << 4) | b) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b'=');
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode RFC 2047 encoded-words like `=?UTF-8?B?...?=` into a UTF-8 string.
/// Handles `B` (base64) and `Q` (quoted-printable) encodings. Non-UTF-8
/// charsets are decoded as if their byte stream were UTF-8 (lossy).
fn decode_mime_header(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut last_was_encoded = false;
    while !rest.is_empty() {
        let Some(start) = rest.find("=?") else {
            out.push_str(rest);
            break;
        };
        let between = &rest[..start];
        if !(last_was_encoded && between.chars().all(char::is_whitespace)) {
            out.push_str(between);
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("?=") else {
            out.push_str("=?");
            rest = after;
            last_was_encoded = false;
            continue;
        };
        let body = &after[..end];
        match decode_encoded_word(body) {
            Some(decoded) => {
                out.push_str(&decoded);
                last_was_encoded = true;
            }
            None => {
                out.push_str("=?");
                out.push_str(body);
                out.push_str("?=");
                last_was_encoded = false;
            }
        }
        rest = &after[end + 2..];
    }
    out
}

fn decode_encoded_word(body: &str) -> Option<String> {
    let mut parts = body.splitn(3, '?');
    let _charset = parts.next()?;
    let enc = parts.next()?;
    let text = parts.next()?;
    let bytes = match enc.to_ascii_uppercase().as_str() {
        "B" => decode_base64(text)?,
        "Q" => decode_q(text),
        _ => return None,
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let val: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn decode_q(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() => {
                let h1 = (bytes[i + 1] as char).to_digit(16);
                let h2 = (bytes[i + 2] as char).to_digit(16);
                if let (Some(a), Some(b)) = (h1, h2) {
                    out.push(((a << 4) | b) as u8);
                    i += 3;
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}
