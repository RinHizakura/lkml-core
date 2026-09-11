// SPDX-License-Identifier: GPL-2.0

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use std::cmp::Reverse;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const BASE: &str = "https://lore.kernel.org";
const UA: &str = concat!("lkml-reader/", env!("CARGO_PKG_VERSION"));

/// The HTTP client used for every network fetch in this module. Private on
/// purpose: callers ask the archive module for epochs or mirrors and let it own
/// the transport, rather than building and threading a client through the app.
fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(UA)
        .gzip(true)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

fn archive_root() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(base).join("lkml-tools/archives")
}

fn local_repo_path(list: &str, epoch: u32) -> PathBuf {
    archive_root().join(format!("{list}/{epoch}.git"))
}

fn fetch_manifest(client: &reqwest::blocking::Client) -> Result<String> {
    let url = format!("{BASE}/manifest.js.gz");
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("manifest fetch failed ({}): {url}", resp.status());
    }
    let bytes = resp.bytes().context("reading manifest body")?;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .context("decompressing manifest")?;
    Ok(out)
}

fn manifest_epochs(json: &str, list: &str) -> Vec<u32> {
    let prefix = format!("\"/{}/git/", list);
    let mut epochs = std::collections::BTreeSet::new();
    for (i, _) in json.match_indices(&prefix) {
        let start = i + prefix.len();
        let end = json[start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|p| start + p)
            .unwrap_or(json.len());
        if start < end {
            if let Ok(n) = json[start..end].parse::<u32>() {
                epochs.insert(n);
            }
        }
    }
    epochs.into_iter().collect()
}

/// The epochs cloned locally for `list`, oldest-first: every `<n>.git` under
/// the list's cache dir. What the mirror knows without the network.
fn local_epochs(list: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(archive_root().join(list)) else {
        return Vec::new();
    };
    let mut epochs: Vec<u32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".git")?
                .parse::<u32>()
                .ok()
        })
        .collect();
    epochs.sort_unstable();
    epochs
}

/// A list's mirror: the epochs lore publishes for it and, per epoch, whether a
/// local clone exists. This is where the epoch lifecycle lives — which epochs
/// there are, which are present, and how one is made present (clone vs
/// update) — so neither app has to track it. Cheap to clone: it holds only
/// the list name and epoch numbers.
#[derive(Clone, Debug)]
pub struct Mirror {
    list: String,
    /// Oldest-first, as lore's manifest lists them.
    epochs: Vec<u32>,
}

impl Mirror {
    /// The mirror for `list` as lore's manifest describes it. Hits the network
    /// once; errors if the list has no epochs (typically a misspelled name).
    pub fn open(list: &str) -> Result<Mirror> {
        let client = http_client()?;
        let manifest = fetch_manifest(&client)?;
        let epochs = manifest_epochs(&manifest, list);
        if epochs.is_empty() {
            bail!("no epochs found for list '{list}'");
        }
        Ok(Mirror {
            list: list.to_string(),
            epochs,
        })
    }

    /// The mirror for `list` as the local cache alone describes it: only the
    /// epochs already cloned. No network — the fallback when [`Mirror::open`]
    /// cannot reach lore.
    pub fn local(list: &str) -> Mirror {
        Mirror {
            list: list.to_string(),
            epochs: local_epochs(list),
        }
    }

    pub fn list(&self) -> &str {
        &self.list
    }

    /// Every epoch of the list, oldest-first.
    pub fn epochs(&self) -> &[u32] {
        &self.epochs
    }

    /// The newest epoch: the one still receiving mail.
    pub fn newest(&self) -> Option<u32> {
        self.epochs.last().copied()
    }

    pub fn is_cloned(&self, epoch: u32) -> bool {
        local_repo_path(&self.list, epoch).exists()
    }

    /// Make `epoch` present and current locally, choosing how on its own:
    /// refresh it with `git remote update` if already cloned, otherwise `git
    /// clone --mirror` it. Callers name the epoch they want and leave the
    /// clone-vs-update decision — and which git invocation it implies — here.
    pub fn ensure(&self, epoch: u32) -> Result<()> {
        if self.is_cloned(epoch) {
            update_mirror(&self.list, epoch)
        } else {
            clone_mirror(&self.list, epoch)
        }
    }

    /// The epochs to search for a window reaching back to `window_start`,
    /// newest first. Always refreshes the newest epoch; then, while the oldest
    /// epoch held so far still *begins after* `window_start`, takes the next
    /// earlier epoch — stopping once the window is covered or the list's first
    /// epoch is reached.
    ///
    /// Before cloning an epoch that is not yet local, `consent(epoch)` is
    /// asked; a `false` stops the walk there and returns what is covered so
    /// far. Cloning blocks and earlier epochs are large, so that is the
    /// caller's moment to say so on its own output — this module writes to
    /// nothing but the cache.
    pub fn epochs_covering(
        &self,
        window_start: DateTime<Utc>,
        consent: &mut dyn FnMut(u32) -> Result<bool>,
    ) -> Result<Vec<u32>> {
        let mut used = Vec::new();
        for &epoch in self.epochs.iter().rev() {
            if !self.is_cloned(epoch) && !consent(epoch)? {
                break;
            }
            self.ensure(epoch)?;
            used.push(epoch);
            let started = epoch_start_date(&self.list, epoch)?;
            if started.is_some_and(|d| d <= window_start) {
                break;
            }
        }
        Ok(used)
    }
}

fn repo_url(list: &str, epoch: u32) -> String {
    format!("{BASE}/{list}/git/{epoch}.git")
}

/// Run git against the local mirror of `list`'s `epoch` and hand back its
/// stdout. Every read of a mirror goes through here, so they all fail the same
/// way: a non-zero exit becomes an error carrying git's own stderr. (Cloning is
/// the exception — there is no mirror to point `--git-dir` at yet.)
fn git(list: &str, epoch: u32, args: &[&str]) -> Result<String> {
    let dir = local_repo_path(list, epoch);
    let out = Command::new("git")
        .arg(format!("--git-dir={}", dir.display()))
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn update_mirror(list: &str, epoch: u32) -> Result<()> {
    git(list, epoch, &["remote", "update"]).map(drop)
}

fn clone_mirror(list: &str, epoch: u32) -> Result<()> {
    let dir = local_repo_path(list, epoch);
    if dir.exists() {
        bail!("mirror already exists: {}", dir.display());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).context("creating cache dir")?;
    }
    let url = repo_url(list, epoch);
    let out = Command::new("git")
        .arg("clone")
        .arg("--mirror")
        .arg(&url)
        .arg(&dir)
        .output()
        .context("running git clone --mirror")?;
    if !out.status.success() {
        bail!(
            "git clone --mirror failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// When this epoch started archiving — the minimum *committer* date (`%ct`)
/// across all commits, or `None` if the repo has no commits. public-inbox sets
/// the committer date to the moment it imported the mail, so it is monotonic
/// per epoch and immune to the bogus `Date:` headers that pepper big lists
/// (a single mail mis-dated to 1999 would otherwise make every epoch look like
/// it reaches back forever). Used to decide whether a query window reaches back
/// before this epoch's coverage — if this date is at or before the window
/// start, the epoch covers the start and no earlier epoch is needed.
pub fn epoch_start_date(list: &str, epoch: u32) -> Result<Option<DateTime<Utc>>> {
    let min = git(list, epoch, &["log", "--pretty=format:%ct"])?
        .lines()
        .filter_map(|l| l.trim().parse::<i64>().ok())
        .min();
    Ok(min.and_then(|ts| DateTime::from_timestamp(ts, 0)))
}

/// All commits in the repo, newest first by the mail's own `Date:` header.
/// public-inbox records that header as the git *author* date, so sorting on it
/// (`%at`) gives a globally date-ordered list — across pages, not just within
/// one — without reading any mail bodies. (git-log's natural order is the
/// archival/committer date, which can shuffle a batch received in one second.)
pub fn list_all_commits(list: &str, epoch: u32) -> Result<Vec<String>> {
    search_commits(list, epoch, None, None)
}

/// Commits whose mail matches the given subject and/or author substrings.
/// Both needles are case-insensitive fixed strings, and are ANDed when
/// both are given.
pub fn search_commits(
    list: &str,
    epoch: u32,
    subject: Option<&str>,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let grep = subject.map(|needle| format!("--grep={needle}"));
    let author = author.map(|needle| format!("--author={needle}"));
    let mut args = vec!["log", "--pretty=format:%H %at"];
    if grep.is_some() || author.is_some() {
        // Fixed strings, case-insensitive: same semantics as a lowercased
        // `contains` over the header.
        args.extend(["--fixed-strings", "--regexp-ignore-case"]);
    }
    args.extend(grep.iter().chain(author.iter()).map(String::as_str));

    let mut rows: Vec<(i64, String)> = git(list, epoch, &args)?
        .lines()
        .filter_map(|l| {
            let (hash, ts) = l.split_once(' ')?;
            Some((ts.trim().parse::<i64>().unwrap_or(0), hash.to_string()))
        })
        .collect();
    // Newest author-date (mail Date) first.
    rows.sort_by_key(|&(ts, _)| Reverse(ts));
    Ok(rows.into_iter().map(|(_, hash)| hash).collect())
}

/// List commits whose committer date is at or after `since`. Uses
/// `git log --since` so it can prune a large epoch quickly for time-windowed
/// queries (e.g. "last 24 hours").
pub fn list_commits_since(
    list: &str,
    epoch: u32,
    since: DateTime<chrono::Utc>,
) -> Result<Vec<String>> {
    let since = format!("--since={}", since.format("%Y-%m-%d %H:%M:%S +0000"));
    Ok(git(list, epoch, &["log", "--pretty=format:%H", &since])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Read the mails at `commits` — their public-inbox `m` blobs — in one
/// `git cat-file --batch`, in the order asked for.
///
/// Answers come back in request order, so the result is index-aligned with
/// `commits`: a commit whose blob will not read is `None`, rather than a gap that
/// would shift every mail after it onto the wrong commit.
pub(crate) fn show_mails(
    list: &str,
    epoch: u32,
    commits: &[String],
) -> Result<Vec<Option<String>>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let dir = local_repo_path(list, epoch);
    let mut child = Command::new("git")
        .arg(format!("--git-dir={}", dir.display()))
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning git cat-file --batch")?;

    // Feed the requests from a thread: git writes each blob as it reads the
    // request for it, so a caller that wrote every request before reading a byte
    // of output would deadlock the moment the blobs outgrew the stdout pipe.
    let requests: String = commits.iter().map(|c| format!("{c}:m\n")).collect();
    let mut stdin = child.stdin.take().context("git cat-file stdin")?;
    let feeder = std::thread::spawn(move || stdin.write_all(requests.as_bytes()));

    let out = child
        .wait_with_output()
        .context("running git cat-file --batch")?;
    // The feeder only fails when git died early, which the exit status covers.
    let _ = feeder.join();
    if !out.status.success() {
        bail!("git cat-file --batch failed");
    }
    let mut blobs = split_batch(&out.stdout);
    // Git answers every request; pad anyway so a truncated read can only lose
    // mails, never shift the ones after it onto the wrong commit.
    blobs.resize(commits.len(), None);
    Ok(blobs)
}

/// Split `git cat-file --batch` output into one entry per answer, in request
/// order. Every answer opens with a header line: `<oid> <type> <size>` for a
/// hit, or `<request> missing` for a miss, which is all a miss gets. A hit is
/// then `size` bytes of content and a newline.
fn split_batch(mut bytes: &[u8]) -> Vec<Option<String>> {
    let mut out = Vec::new();
    while let Some(eol) = bytes.iter().position(|&b| b == b'\n') {
        let header = String::from_utf8_lossy(&bytes[..eol]);
        let size = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<usize>().ok());
        bytes = &bytes[eol + 1..];
        let Some(size) = size else {
            out.push(None); // a miss: no content follows the header
            continue;
        };
        let Some(content) = bytes.get(..size) else {
            break; // truncated output; keep what we have
        };
        out.push(Some(String::from_utf8_lossy(content).into_owned()));
        bytes = &bytes[(size + 1).min(bytes.len())..];
    }
    out
}
