// SPDX-License-Identifier: GPL-2.0

//! Reply-thread math over a set of mails: given each mail's own Message-ID and
//! the id it replies to, size the reply subtree rooted at each one. Also where
//! a patch series is pulled back out of the archive, since a series is just the
//! patch mails of one thread.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::archive;
use crate::mail::{self, Mail};
use crate::parse::normalize_message_id;

/// What identifies the series a patch belongs to: the thread it hangs off, plus
/// the revision and length of its `[PATCH vV n/m]` tag. Two mails are siblings
/// iff their [`SeriesTag`]s are equal.
///
/// This is [`PatchTag`](crate::mail::PatchTag) minus `number` — which is what
/// tells siblings apart — plus the thread root, since one thread often carries
/// several revisions of a series.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesTag {
    /// The thread root's Message-ID: the cover letter of the series.
    pub root: String,
    pub version: u32,
    pub total: u32,
}

/// The normalized Message-ID of the thread `mail` hangs off: the first
/// `References` entry, else the mail it replies to, else itself — a thread root
/// (a cover letter has no References and is its own root). Mails of one thread
/// share this key, so grouping by it rebuilds threads without parsing subjects.
pub fn thread_root(mail: &Mail) -> String {
    let root = mail
        .references
        .first()
        .map(String::as_str)
        .or_else(|| (!mail.in_reply_to.is_empty()).then_some(mail.in_reply_to.as_str()))
        .unwrap_or(&mail.message_id);
    normalize_message_id(root)
}

/// The series `mail` belongs to, or `None` when it is not part of a multi-patch
/// series — ordinary mail, a review reply, or a lone `[PATCH]`.
pub fn series_tag(mail: &Mail) -> Option<SeriesTag> {
    let tag = mail.patch_tag.filter(|t| t.total > 1)?;
    Some(SeriesTag {
        root: thread_root(mail),
        version: tag.version,
        total: tag.total,
    })
}

/// Reorder `mails` so every patch series forms one block — cover letter (or
/// lowest-numbered patch) first, the rest ascending — sitting where the series'
/// newest mail was, and flag the members that belong under the head. A series
/// with only one member present stays where it is, unflagged: there is nothing
/// to indent it under.
pub fn group_series(mails: &[Mail]) -> (Vec<Mail>, Vec<bool>) {
    let tags: Vec<Option<SeriesTag>> = mails.iter().map(series_tag).collect();
    let mut series: HashMap<&SeriesTag, Vec<usize>> = HashMap::new();
    for (i, tag) in tags.iter().enumerate() {
        if let Some(tag) = tag {
            series.entry(tag).or_default().push(i);
        }
    }
    for members in series.values_mut() {
        members.sort_by_key(|&i| mails[i].patch_tag.map_or(0, |t| t.number));
    }

    let mut out = Vec::with_capacity(mails.len());
    let mut indent = Vec::with_capacity(mails.len());
    let mut placed = vec![false; mails.len()];
    for i in 0..mails.len() {
        if placed[i] {
            continue;
        }
        let block = match tags[i].as_ref().and_then(|tag| series.get(tag)) {
            Some(members) if members.len() > 1 => members.as_slice(),
            _ => std::slice::from_ref(&i),
        };
        for (nth, &j) in block.iter().enumerate() {
            placed[j] = true;
            out.push(mails[j].clone());
            indent.push(nth > 0);
        }
    }
    (out, indent)
}

/// Is every patch of `tag` among `mails`? The 0/m cover letter is optional;
/// 1/m..m/m are not.
pub fn is_whole(mails: &[Mail], tag: &SeriesTag) -> bool {
    let seen: HashSet<u32> = mails
        .iter()
        .filter(|mail| series_tag(mail).as_ref() == Some(tag))
        .filter_map(|mail| mail.patch_tag.map(|patch| patch.number))
        .collect();
    (1..=tag.total).all(|n| seen.contains(&n))
}

/// Every patch of `sel`'s series, ordered 1/m, 2/m, …, wherever the mails sit
/// in the archive. Empty when `sel` is not a patch mail.
///
/// A mail belongs to the series when it shares `sel`'s revision *and* series
/// length *and* thread, and carries a real `[PATCH n/m]` (not the `0/m` cover).
/// All three matter: the same thread often holds several revisions of a series,
/// and a bare `[PATCH]` fixup posted as a reply would otherwise pose as `1/1`
/// and shoulder out the real first patch.
pub fn patch_series(list: &str, sel: &Mail) -> Result<Vec<Mail>> {
    let Some(sel_tag) = sel.patch_tag.filter(|t| t.number > 0) else {
        return Ok(Vec::new());
    };
    let root = thread_root(sel);

    // Let git log prune the epoch before any mail is read.
    // TODO: only the selected mail's epoch is searched, and only that
    // sender's mails — a series straddling an epoch boundary, or one resent
    // under a different From spelling, loses the stragglers.
    let commits = archive::search_commits(list, sel.epoch, Some("PATCH"), Some(&sel.sender))
        .context("searching the mirror for the rest of the series")?;

    let mut series: BTreeMap<u32, Mail> = BTreeMap::new();
    for mail in mail::read(list, sel.epoch, &commits) {
        let Some(tag) = mail.patch_tag else {
            continue;
        };
        if tag.number == 0
            || tag.version != sel_tag.version
            || tag.total != sel_tag.total
            || !references_root(&mail, &root)
        {
            continue;
        }
        // search_commits answers newest-first, so a resend beats the original.
        series.entry(tag.number).or_insert(mail);
    }
    // The selected mail is ground truth: keep it even when the pre-filter missed
    // it (an oddly spelled From, say).
    series.entry(sel_tag.number).or_insert_with(|| sel.clone());

    // Keys are the patch numbers, so this comes out in apply order.
    Ok(series.into_values().collect())
}

fn references_root(mail: &Mail, root: &str) -> bool {
    normalize_message_id(&mail.message_id) == root
        || mail
            .references
            .iter()
            .any(|r| normalize_message_id(r) == root)
}

/// For each item, the number of items in the set that reply to it transitively
/// (its thread-subtree size minus itself). Only items within `items` are
/// counted, so a thread root reflects the in-set thread size. The result is
/// index-aligned with `items`.
pub fn reply_counts(items: &[Mail]) -> Vec<usize> {
    let mut id_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if !it.message_id.is_empty() {
            id_to_idx
                .entry(normalize_message_id(&it.message_id))
                .or_insert(i);
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    for (i, it) in items.iter().enumerate() {
        if it.in_reply_to.is_empty() {
            continue;
        }
        if let Some(&p) = id_to_idx.get(&normalize_message_id(&it.in_reply_to)) {
            if p != i {
                children[p].push(i);
            }
        }
    }
    let mut memo: Vec<Option<usize>> = vec![None; items.len()];
    let mut on_stack = vec![false; items.len()];
    (0..items.len())
        .map(|i| subtree_size(i, &children, &mut memo, &mut on_stack).saturating_sub(1))
        .collect()
}

/// Memoized subtree size with a stack guard so malformed reply cycles can't
/// recurse forever.
fn subtree_size(
    i: usize,
    children: &[Vec<usize>],
    memo: &mut Vec<Option<usize>>,
    on_stack: &mut Vec<bool>,
) -> usize {
    if let Some(v) = memo[i] {
        return v;
    }
    if on_stack[i] {
        return 0;
    }
    on_stack[i] = true;
    let mut total = 1;
    for c in 0..children[i].len() {
        total += subtree_size(children[i][c], children, memo, on_stack);
    }
    on_stack[i] = false;
    memo[i] = Some(total);
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::PatchTag;

    /// Patch `number/total` (v1) of the series rooted at `<root>`; `number` 0
    /// is the cover letter, so it is its own root.
    fn patch(root: &str, number: u32, total: u32) -> Mail {
        Mail {
            subject: format!("[{root} {number}/{total}]"),
            message_id: if number == 0 {
                format!("<{root}>")
            } else {
                format!("<{root}.{number}>")
            },
            references: if number == 0 {
                Vec::new()
            } else {
                vec![format!("<{root}>")]
            },
            patch_tag: Some(PatchTag {
                version: 1,
                number,
                total,
            }),
            ..Mail::default()
        }
    }

    fn plain(subject: &str) -> Mail {
        Mail {
            subject: subject.to_string(),
            ..Mail::default()
        }
    }

    fn subjects(mails: &[Mail]) -> Vec<&str> {
        mails.iter().map(|m| m.subject.as_str()).collect()
    }

    #[test]
    fn series_forms_a_block_where_its_newest_mail_sat() {
        let mails = vec![
            patch("a", 2, 3),
            plain("x"),
            patch("a", 1, 3),
            patch("a", 3, 3),
        ];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 1/3]", "[a 2/3]", "[a 3/3]", "x"]);
        assert_eq!(indent, [false, true, true, false]);
    }

    #[test]
    fn cover_letter_heads_its_block() {
        let mails = vec![patch("a", 1, 2), patch("a", 0, 2), patch("a", 2, 2)];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 0/2]", "[a 1/2]", "[a 2/2]"]);
        assert_eq!(indent, [false, true, true]);
    }

    #[test]
    fn two_series_group_independently() {
        let mails = vec![
            patch("a", 2, 2),
            patch("b", 2, 2),
            patch("b", 1, 2),
            patch("a", 1, 2),
        ];
        let (out, _) = group_series(&mails);
        assert_eq!(subjects(&out), ["[a 1/2]", "[a 2/2]", "[b 1/2]", "[b 2/2]"]);
    }

    #[test]
    fn stray_member_and_lone_patch_stay_put() {
        // Only 2/9 of its series is here, and a lone [PATCH 1/1] is no series:
        // nothing to pull together, nothing indented.
        let mails = vec![plain("x"), patch("s", 2, 9), patch("l", 1, 1)];
        let (out, indent) = group_series(&mails);
        assert_eq!(subjects(&out), ["x", "[s 2/9]", "[l 1/1]"]);
        assert_eq!(indent, [false, false, false]);
    }

    #[test]
    fn is_whole_ignores_missing_cover_and_other_series() {
        let mails = vec![patch("a", 1, 2), patch("b", 2, 2), patch("a", 2, 2)];
        let tag_a = series_tag(&mails[0]).unwrap();
        assert!(is_whole(&mails, &tag_a)); // 1..=2 present; no cover needed
        let tag_b = series_tag(&mails[1]).unwrap();
        assert!(!is_whole(&mails, &tag_b)); // b is missing 1/2
    }

    #[test]
    fn thread_root_falls_back_to_in_reply_to_then_self() {
        let mut m = plain("x");
        m.message_id = "<self>".into();
        assert_eq!(thread_root(&m), "self");
        m.in_reply_to = "<parent>".into();
        assert_eq!(thread_root(&m), "parent");
        m.references = vec!["<root>".into(), "<parent>".into()];
        assert_eq!(thread_root(&m), "root");
    }

    #[test]
    fn reply_counts_size_each_subtree() {
        let mut root = plain("r");
        root.message_id = "<r>".into();
        let mut a = plain("a");
        a.message_id = "<a>".into();
        a.in_reply_to = "<r>".into();
        let mut b = plain("b");
        b.message_id = "<b>".into();
        b.in_reply_to = "<a>".into();
        assert_eq!(reply_counts(&[root, a, b]), [2, 1, 0]);
    }
}
