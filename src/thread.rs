// SPDX-License-Identifier: GPL-2.0

//! Reply-thread math over a set of mails: given each mail's own Message-ID and
//! the id it replies to, size the reply subtree rooted at each one. Also where
//! a patch series is pulled back out of the archive, since a series is just the
//! patch mails of one thread.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};

use crate::archive;
use crate::mail::{self, Mail};
use crate::parse::normalize_message_id;

/// Anything that can sit in a reply tree: it knows its own id and its parent's.
/// Implemented for [`Mail`]; apps can implement it for their own wrapper types
/// so [`reply_counts`] works without copying mails out of them.
pub trait Threaded {
    fn message_id(&self) -> &str;
    fn in_reply_to(&self) -> &str;
}

impl Threaded for Mail {
    fn message_id(&self) -> &str {
        &self.message_id
    }
    fn in_reply_to(&self) -> &str {
        &self.in_reply_to
    }
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
    let root = normalize_message_id(sel.references.first().unwrap_or(&sel.message_id));

    // Let git log prune the epoch before any mail is read.
    // TODO: only the selected mail's epoch is searched, and only that
    // sender's mails — a series straddling an epoch boundary, or one resent
    // under a different From spelling, loses the stragglers.
    let commits = archive::search_commits(list, sel.epoch, Some("PATCH"), Some(&sel.sender))
        .context("searching the mirror for the rest of the series")?;

    let mut series: BTreeMap<u32, Mail> = BTreeMap::new();
    for commit in commits {
        let Ok(mail) = mail::fetch(list, sel.epoch, &commit) else {
            continue;
        };
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
pub fn reply_counts<T: Threaded>(items: &[T]) -> Vec<usize> {
    let mut id_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if !it.message_id().is_empty() {
            id_to_idx
                .entry(normalize_message_id(it.message_id()))
                .or_insert(i);
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    for (i, it) in items.iter().enumerate() {
        if it.in_reply_to().is_empty() {
            continue;
        }
        if let Some(&p) = id_to_idx.get(&normalize_message_id(it.in_reply_to())) {
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
