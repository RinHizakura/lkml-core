// SPDX-License-Identifier: GPL-2.0

//! Reply-thread math over a set of mails: given each mail's own Message-ID and
//! the id it replies to, size the reply subtree rooted at each one.

use std::collections::HashMap;

use crate::mail::Mail;
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
