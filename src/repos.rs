//! Repository discovery, the cached repository list, and cheap change stamps.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// callers skip running git. The index is left out: `git status` and similar commands
    /// rewrite it to refresh cached file metadata, unless run with `GIT_OPTIONAL_LOCKS=0`,
    /// without changing history.
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
/// Adding or removing a project folder changes its root's mtime and triggers a rebuild.
/// Cached repositories whose git directory is gone are dropped on every load, so a removed
/// nested repository disappears at once; new nested repositories appear with the daily
/// rebuild or `--rescan`.
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
        let repos: Vec<Repo> = cache
            .repos
            .iter()
            .filter(|r| r.git_dir.is_dir())
            .cloned()
            .collect();
        if repos.len() < cache.repos.len() {
            save(
                &path,
                &Cache {
                    repos: repos.clone(),
                    ..cache
                },
            );
        }
        return repos;
    }
    let repos = discover(roots, depth);
    save(
        &path,
        &Cache {
            roots: stamps,
            depth,
            written: now(),
            repos: repos.clone(),
        },
    );
    repos
}

/// Drops linked worktrees whose main working tree is also listed. Worktrees share their
/// repository's branches, so each would list the same commits as the main working tree. A
/// repository found only through its worktrees keeps the first one.
pub fn drop_linked_worktrees(repos: Vec<Repo>) -> Vec<Repo> {
    let is_main = |r: &Repo| r.git_dir == r.common_dir;
    let mains: HashSet<PathBuf> = repos
        .iter()
        .filter(|r| is_main(r))
        .map(|r| r.common_dir.clone())
        .collect();
    let mut kept = HashSet::new();
    repos
        .into_iter()
        .filter(|r| {
            is_main(r) || (!mains.contains(&r.common_dir) && kept.insert(r.common_dir.clone()))
        })
        .collect()
}

fn save(path: &Path, cache: &Cache) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let temporary = path.with_extension("tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(&temporary, path);
        }
    }
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
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        newest = newest.max(if is_dir {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(path: &str, git_dir: &str, common_dir: &str) -> Repo {
        Repo {
            path: path.into(),
            git_dir: git_dir.into(),
            common_dir: common_dir.into(),
            identities: Vec::new(),
        }
    }

    #[test]
    fn linked_worktrees_collapse_into_their_repository() {
        let repos = vec![
            repo("/c/a", "/c/a/.git", "/c/a/.git"),
            repo("/c/a-fix", "/c/a/.git/worktrees/a-fix", "/c/a/.git"),
            repo("/c/b-one", "/x/b/.git/worktrees/b-one", "/x/b/.git"),
            repo("/c/b-two", "/x/b/.git/worktrees/b-two", "/x/b/.git"),
        ];
        let paths: Vec<PathBuf> = drop_linked_worktrees(repos)
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(paths, [PathBuf::from("/c/a"), PathBuf::from("/c/b-one")]);
    }
}
