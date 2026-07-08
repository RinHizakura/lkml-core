// SPDX-License-Identifier: GPL-2.0

/// Normalize a Message-ID for comparison: trim whitespace and angle brackets so
/// `<id@host>` and `id@host` compare equal.
pub fn normalize_message_id(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string()
}
