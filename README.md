# lkml-core

Mail parsing and archive access for
[lore.kernel.org](https://lore.kernel.org) public-inbox lists.

## How it works

`lore.kernel.org` runs [public-inbox](https://public-inbox.org/), which exposes
each mailing list as one or more **bare git repositories**. Every commit is a
single email; the mail's body is stored as the blob `m` in the commit's tree.
This library reads mails by cloning those git mirrors and shelling out to
`git log` / `git show`.

### Manifest

`https://lore.kernel.org/manifest.js.gz` is a gzipped JSON catalog (grokmirror
format) of every repo lore serves:

```jsonc
{
  "/lkml/git/0.git":  { "description": "LKML [epoch 0]",  ... },
  "/lkml/git/1.git":  { "description": "LKML [epoch 1]",  ... },
  ...
  "/lkml/git/19.git": { "description": "LKML [epoch 19]", ... }
}
```

`archive::list_epochs` fetches this file, decompresses it, and extracts the
epoch numbers (`0..=19` above) for a given list.

### Epochs

Once a list's git repo grows large, public-inbox rolls a new repo so each one
stays cloneable in a reasonable time. Those numbered slices (`0.git`, `1.git`,
...) are called **epochs**. They're append-only and time-ordered:

- Higher epoch number = newer mail.
- The current epoch is the only one still receiving new commits.
- Older epochs never change once retired.

Consumers start at the highest epoch (newest mails) and roll back to
`epoch - 1` to reach older history. Small lists (e.g. `damon`) only have
`0.git`; lkml currently has 20 epochs.

### Local cache

Mirrors live under:

```
$XDG_CACHE_HOME/lkml-tools/archives/<list>/<epoch>.git
```

(falls back to `~/.cache/lkml-tools/archives/...`). `archive::ensure_epoch`
clones the epoch if missing, otherwise runs `git remote update`. Reading a mail
uses `git log` / `git show` against the local mirror — no network round-trip
once the clone is in place. Older epochs are cloned on demand when a query
reaches before the oldest local mail.

The same cache is shared by every tool built on this library, so running one
keeps the others up to date.
