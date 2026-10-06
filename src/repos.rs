//! Repository discovery, the cached repository list, and cheap change stamps.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Directories never searched for repositories.
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", "__pycache__"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    /// Working tree path.
    pub path: PathBuf,
    /// This working tree's git directory (differs from `common_dir` for linked worktrees).
    pub git_dir: PathBuf,
    /// Directory holding branches and their reflogs, shared by all worktrees.
    pub common_dir: PathBuf,
    /// Author identities counted as the user's own commits.
    pub identities: Vec<String>,
}

impl Repo {
    pub fn name(&self, roots: &[PathBuf]) -> String {
        for root in roots {
            if let Ok(relative) = self.path.strip_prefix(root) {
                let name = relative.to_string_lossy();
                if !name.is_empty() {
                    return name.into_owned();
                }
            }
        }
        self.path.to_string_lossy().into_owned()
    }

    /// Newest modification time, in seconds, of the files Git updates on every commit,
    /// merge, rebase, pull, or branch change. Unchanged stamps mean no new local commits, so
    /// callers skip running git. The index is left out: any `git status`, such as a shell
    /// prompt's, rewrites it without changing history.
    pub fn stamp(&self) -> u64 {
        let mut newest = 0;
        for file in [
            self.git_dir.join("logs/HEAD"),
            self.git_dir.join("HEAD"),
            self.common_dir.join("packed-refs"),
        ] {
            newest = newest.max(mtime(&file));
        }
        newest.max(newest_in(&self.common_dir.join("logs/refs/heads")))
    }
}

#[derive(Serialize, Deserialize)]
struct Cache {
    roots: Vec<(PathBuf, u64)>,
    depth: usize,
    written: u64,
    repos: Vec<Repo>,
}

/// Seconds after which the cached list is rebuilt even if no root changed.
const CACHE_MAX_AGE: u64 = 24 * 60 * 60;

/// Returns the repositories under `roots`, from the cache when no root folder changed.
/// Adding or removing a project folder changes its root's mtime and triggers a rebuild;
/// nested repositories are picked up by the daily rebuild or `--rescan`.
pub fn load(roots: &[PathBuf], depth: usize, rescan: bool) -> Vec<Repo> {
    let path = cache_path();
    let stamps: Vec<(PathBuf, u64)> = roots.iter().map(|r| (r.clone(), mtime(r))).collect();
    if !rescan
        && let Some(cache) = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        && cache.roots == stamps
        && cache.depth == depth
        && now().saturating_sub(cache.written) < CACHE_MAX_AGE
    {
        return cache.repos;
    }
    let repos = discover(roots, depth);
    let cache = Cache {
        roots: stamps,
        depth,
        written: now(),
        repos: repos.clone(),
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let temporary = path.with_extension("tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(&temporary, &path);
        }
    }
    repos
}

fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"));
    base.join("gittop/repos.json")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Walks each root up to `depth` levels and resolves every working tree it finds.
pub fn discover(roots: &[PathBuf], depth: usize) -> Vec<Repo> {
    let mut trees = Vec::new();
    for root in roots {
        walk(root, depth, &mut trees);
    }
    trees.sort();
    trees.dedup();
    let mut repos: Vec<Repo> = std::thread::scope(|scope| {
        let chunks: Vec<_> = trees.chunks(trees.len().div_ceil(16).max(1)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || chunk.iter().filter_map(|t| resolve(t)).collect::<Vec<_>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    repos.sort_by(|a, b| a.path.cmp(&b.path));
    repos
}

/// Collects every folder with a `.git` entry, including repositories nested inside others,
/// since a root such as `/code` may itself hold a `.git` folder. Folders that `git` does not
/// accept as repositories are dropped when resolved.
fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        found.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        walk(&entry.path(), depth - 1, found);
    }
}

fn resolve(tree: &Path) -> Option<Repo> {
    let output = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "--absolute-git-dir", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let git_dir = PathBuf::from(lines.next()?);
    let common = PathBuf::from(lines.next()?);
    let common_dir = if common.is_absolute() {
        common
    } else {
        tree.join(common)
    };
    let common_dir = common_dir.canonicalize().unwrap_or(common_dir);
    let mut identities = Vec::new();
    for key in ["user.name", "user.email"] {
        if let Ok(out) = Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(["config", key])
            .output()
        {
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !value.is_empty() {
                identities.push(value);
            }
        }
    }
    Some(Repo {
        path: tree.to_path_buf(),
        git_dir,
        common_dir,
        identities,
    })
}

pub fn mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn newest_in(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut newest = mtime(dir);
    for entry in entries.flatten() {
        let path = entry.path();
        newest = newest.max(if path.is_dir() {
            newest_in(&path)
        } else {
            mtime(&path)
        });
    }
    newest
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
