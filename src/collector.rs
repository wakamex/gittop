//! Background collection: waits for the files Git updates on each commit to change, and runs
//! git only in repositories whose change stamp moved, so the UI thread never waits on git and
//! nothing runs while no repository changes. It also watches the searched folders, so
//! repositories created or removed while it runs are added or dropped.

use crate::activity::{self, Activity};
use crate::repos::{self, Repo};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

pub struct Settings {
    pub window_secs: u64,
    pub me: Vec<String>,
    pub limit: usize,
    /// How often repositories that could not be watched are checked.
    pub interval: Duration,
    pub roots: Vec<PathBuf>,
    pub depth: usize,
    /// List linked worktrees as their own rows.
    pub worktrees: bool,
}

pub enum Update {
    Activity(usize, Activity),
    /// A repository appeared under the roots; it takes the next index.
    Added(Repo),
    /// The repository at this index was removed.
    Gone(usize),
    /// The first pass over every repository finished.
    Ready,
}

pub enum Request {
    /// Re-read one repository now.
    Refresh(usize),
    Pause(bool),
    Window(u64),
    /// A watched folder changed.
    Files(notify::Result<notify::Event>),
}

const WORKERS: usize = 16;

/// Wait after a change before reading, so the several files one commit writes cost one read.
const SETTLE: Duration = Duration::from_millis(100);

/// How long a `.git` entry that is not yet a complete repository, as during `git clone`, is
/// retried.
const PENDING_LIMIT: Duration = Duration::from_secs(60);

pub fn run(
    all: Vec<Repo>,
    listed: Vec<Repo>,
    settings: Settings,
    updates: Sender<Update>,
    requests: Receiver<Request>,
    sender: Sender<Request>,
) {
    let mut settings = settings;
    let mut list = List {
        all,
        listed,
        gone: HashSet::new(),
    };
    let mut watches = Watches::new(sender);
    let mut stamps: HashMap<usize, u64> = HashMap::new();
    let mut dirty: HashSet<usize> = (0..list.listed.len()).collect();
    let mut forced = HashSet::new();
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let mut paused = false;
    let mut first = true;
    let mut next_poll = Instant::now();
    loop {
        if Instant::now() >= next_poll {
            dirty.extend(&watches.polled);
            next_poll = Instant::now() + settings.interval;
        }
        if !paused || !forced.is_empty() {
            let check: Vec<usize> = if paused {
                forced.iter().copied().collect()
            } else {
                dirty.union(&forced).copied().collect()
            };
            let check: Vec<usize> = check
                .into_iter()
                .filter(|i| !list.gone.contains(i))
                .collect();
            // Watch before reading the stamp, so a change between the two is never missed.
            for &i in &check {
                watches.add(i, &list.listed[i]);
            }
            let changed: Vec<usize> = check
                .into_iter()
                .filter(|&i| {
                    let stamp = list.listed[i].stamp();
                    let fresh = stamps.get(&i) != Some(&stamp);
                    if fresh && !paused {
                        stamps.insert(i, stamp);
                    }
                    (fresh && !paused) || forced.contains(&i)
                })
                .collect();
            if !paused {
                dirty.clear();
            }
            forced.clear();
            let since = repos::now().saturating_sub(settings.window_secs);
            let read = |&i: &usize| {
                (
                    i,
                    activity::read(&list.listed[i], since, &settings.me, settings.limit),
                )
            };
            if parallel(&changed, read, |(i, a)| {
                updates.send(Update::Activity(i, a)).is_ok()
            })
            .is_err()
            {
                return;
            }
            if first {
                first = false;
                if updates.send(Update::Ready).is_err() {
                    return;
                }
                // Watch every searched folder, and bring the possibly stale cached list up to
                // date with what is on disk now.
                let (folders, found) = repos::scan(&settings.roots, settings.depth);
                for (folder, depth) in folders {
                    watches.watch_tree(folder, depth);
                }
                let paths: HashSet<PathBuf> = found.iter().map(|r| r.path.clone()).collect();
                let mut changes = list.remove(|r| !paths.contains(&r.path));
                for repo in found {
                    changes.extend(list.add(repo, settings.worktrees));
                }
                if apply(changes, &list, &mut watches, &mut dirty, &updates).is_err() {
                    return;
                }
                repos::store(&settings.roots, settings.depth, &list.all);
                continue;
            }
        }

        let first_request = if watches.polled.is_empty() && pending.is_empty() {
            requests.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            let poll = if watches.polled.is_empty() {
                Duration::MAX
            } else {
                next_poll.saturating_duration_since(Instant::now())
            };
            requests.recv_timeout(poll.min(Duration::from_secs(1)))
        };
        let first_request = match first_request {
            Ok(request) => Some(request),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if first_request.is_some() {
            std::thread::sleep(SETTLE);
        }
        // Folders added or removed under the roots, with the levels still searched below them.
        let mut moved: HashMap<PathBuf, usize> = HashMap::new();
        for request in first_request.into_iter().chain(requests.try_iter()) {
            match request {
                Request::Refresh(index) => {
                    forced.insert(index);
                }
                Request::Pause(value) => paused = value,
                Request::Window(secs) => {
                    settings.window_secs = secs;
                    stamps.clear();
                    dirty.extend(0..list.listed.len());
                }
                // Opening a file, as reading a stamp does, changes nothing.
                Request::Files(Ok(event)) if matches!(event.kind, EventKind::Access(_)) => {}
                Request::Files(Ok(event)) if !event.need_rescan() => {
                    for path in &event.paths {
                        dirty.extend(watches.owners(path));
                        if let Some(depth) = watches.tree_depth(path) {
                            moved.insert(path.clone(), depth);
                        }
                    }
                }
                Request::Files(_) => dirty.extend(0..list.listed.len()),
            }
        }

        let mut changes = Vec::new();
        for (path, depth) in moved {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == ".git" {
                let folder = path.parent().unwrap_or(&path).to_path_buf();
                if path.exists() {
                    pending.insert(folder, Instant::now());
                } else {
                    changes.extend(list.remove(|r| r.path == folder));
                }
            } else if path.is_dir() {
                if depth > 0 && repos::searched(&name) {
                    let (folders, found) = repos::scan(std::slice::from_ref(&path), depth - 1);
                    for (folder, depth) in folders {
                        // A `.git` still being written, as by `git clone`, is retried.
                        if folder.join(".git").exists() && !found.iter().any(|r| r.path == folder) {
                            pending.insert(folder.clone(), Instant::now());
                        }
                        watches.watch_tree(folder, depth);
                    }
                    for repo in found {
                        changes.extend(list.add(repo, settings.worktrees));
                    }
                }
            } else if !path.exists()
                && (watches.trees.contains_key(&path)
                    || list.all.iter().any(|r| r.path.starts_with(&path)))
            {
                // A removed searched folder, not one of the many files removed inside them.
                watches.forget_trees(&path);
                changes.extend(list.remove(|r| r.path.starts_with(&path)));
            }
        }
        pending.retain(|folder, since| match repos::resolve(folder) {
            Some(repo) => {
                changes.extend(list.add(repo, settings.worktrees));
                false
            }
            None => folder.join(".git").exists() && since.elapsed() < PENDING_LIMIT,
        });
        if !changes.is_empty() {
            if apply(changes, &list, &mut watches, &mut dirty, &updates).is_err() {
                return;
            }
            if watches.complete {
                repos::store(&settings.roots, settings.depth, &list.all);
            }
        }
    }
}

/// A change to the listed repositories.
enum Change {
    Added(usize),
    Gone(usize),
}

/// Tells the UI about added and removed repositories, and queues added ones for a first read.
fn apply(
    changes: Vec<Change>,
    list: &List,
    watches: &mut Watches,
    dirty: &mut HashSet<usize>,
    updates: &Sender<Update>,
) -> Result<(), ()> {
    for change in changes {
        let update = match change {
            Change::Added(i) => {
                dirty.insert(i);
                Update::Added(list.listed[i].clone())
            }
            Change::Gone(i) => {
                watches.forget(i);
                dirty.remove(&i);
                Update::Gone(i)
            }
        };
        updates.send(update).map_err(|_| ())?;
    }
    Ok(())
}

/// Every repository found under the roots, which is what the cache records, and the listed
/// ones. A listed repository keeps its index for the whole session; a removed one is marked
/// gone.
struct List {
    all: Vec<Repo>,
    listed: Vec<Repo>,
    gone: HashSet<usize>,
}

impl List {
    /// Adds a repository found while running. A linked worktree is listed only with
    /// `worktrees` or when no listed repository shares its branches.
    fn add(&mut self, repo: Repo, worktrees: bool) -> Option<Change> {
        if self.all.iter().any(|r| r.path == repo.path) {
            return None;
        }
        self.all.push(repo.clone());
        let shadowed = !worktrees
            && repo.git_dir != repo.common_dir
            && self
                .listed
                .iter()
                .enumerate()
                .any(|(i, r)| !self.gone.contains(&i) && r.common_dir == repo.common_dir);
        if shadowed {
            return None;
        }
        self.listed.push(repo);
        Some(Change::Added(self.listed.len() - 1))
    }

    /// Drops the repositories that `affected` selects and that are no longer repositories.
    fn remove(&mut self, affected: impl Fn(&Repo) -> bool) -> Vec<Change> {
        let removed = |r: &Repo| affected(r) && repos::resolve(&r.path).is_none();
        self.all.retain(|r| !removed(r));
        let gone: Vec<usize> = (0..self.listed.len())
            .filter(|i| !self.gone.contains(i) && removed(&self.listed[*i]))
            .collect();
        self.gone.extend(&gone);
        gone.into_iter().map(Change::Gone).collect()
    }
}

/// Folder watches for each repository's change-stamp files. Repositories whose folders cannot
/// be watched, for example past the system's watch limit, are checked every interval instead.
struct Watches {
    watcher: Option<RecommendedWatcher>,
    /// Watched folder, whether it is a git directory, and the repositories it belongs to.
    folders: HashMap<PathBuf, (bool, Vec<usize>)>,
    polled: HashSet<usize>,
    /// Watched searched folders, with how many levels below each are still searched.
    trees: HashMap<PathBuf, usize>,
    /// Whether every searched folder is watched, so the list on disk stays current.
    complete: bool,
}

impl Watches {
    fn new(sender: Sender<Request>) -> Self {
        let watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(Request::Files(event));
        })
        .ok();
        Watches {
            complete: watcher.is_some(),
            watcher,
            folders: HashMap::new(),
            polled: HashSet::new(),
            trees: HashMap::new(),
        }
    }

    fn watch_tree(&mut self, folder: PathBuf, depth: usize) {
        if let Some(known) = self.trees.get_mut(&folder) {
            *known = (*known).max(depth);
            return;
        }
        let watched = self
            .watcher
            .as_mut()
            .is_some_and(|w| w.watch(&folder, RecursiveMode::NonRecursive).is_ok());
        if watched {
            self.trees.insert(folder, depth);
        } else {
            self.complete = false;
        }
    }

    /// For a change directly inside a searched folder, the levels still searched below that
    /// folder.
    fn tree_depth(&self, path: &Path) -> Option<usize> {
        self.trees.get(path.parent()?).copied()
    }

    /// Stops tracking searched folders at or below a removed `path`.
    fn forget_trees(&mut self, path: &Path) {
        self.trees.retain(|folder, _| !folder.starts_with(path));
    }

    /// Stops watching a removed repository's folders, so a repository later created at the
    /// same path is watched again.
    fn forget(&mut self, index: usize) {
        self.polled.remove(&index);
        let watcher = &mut self.watcher;
        self.folders.retain(|folder, (_, owners)| {
            owners.retain(|&owner| owner != index);
            if owners.is_empty()
                && let Some(w) = watcher
            {
                let _ = w.unwatch(folder);
            }
            !owners.is_empty()
        });
    }

    /// Watches whichever of the repository's stamp folders exist and are not yet watched.
    /// Calling this again after a folder appears, such as `logs` after the first commit,
    /// adds it.
    fn add(&mut self, index: usize, repo: &Repo) {
        let Some(watcher) = &mut self.watcher else {
            self.polled.insert(index);
            return;
        };
        for (folder, git_dir, mode) in [
            (repo.git_dir.clone(), true, RecursiveMode::NonRecursive),
            (
                repo.git_dir.join("logs"),
                false,
                RecursiveMode::NonRecursive,
            ),
            (repo.common_dir.clone(), true, RecursiveMode::NonRecursive),
            (
                repo.common_dir.join("logs/refs/heads"),
                false,
                RecursiveMode::Recursive,
            ),
        ] {
            if let Some((_, owners)) = self.folders.get_mut(&folder) {
                if !owners.contains(&index) {
                    owners.push(index);
                }
                continue;
            }
            if !folder.is_dir() {
                continue;
            }
            if watcher.watch(&folder, mode).is_err() {
                self.polled.insert(index);
                return;
            }
            self.folders.insert(folder, (git_dir, vec![index]));
        }
    }

    /// Repositories whose change stamp a change at `path` may have moved. In a git directory
    /// only `HEAD`, `packed-refs`, and a new `logs` folder count, so the index rewrites that
    /// `git status` makes there are ignored.
    fn owners(&self, path: &Path) -> Vec<usize> {
        for folder in path.ancestors().skip(1) {
            if let Some((git_dir, owners)) = self.folders.get(folder) {
                let stamp_file = path.parent() != Some(folder)
                    || !git_dir
                    || path.file_name().is_some_and(|name| {
                        name == "HEAD" || name == "packed-refs" || name == "logs"
                    });
                return if stamp_file {
                    owners.clone()
                } else {
                    Vec::new()
                };
            }
        }
        Vec::new()
    }
}

/// Runs `work` over `items` on up to `WORKERS` threads and passes each result to `send`.
/// Returns an error once `send` reports that the receiver has gone.
fn parallel<T: Send>(
    items: &[usize],
    work: impl Fn(&usize) -> T + Sync,
    mut send: impl FnMut(T) -> bool,
) -> Result<(), ()> {
    if items.is_empty() {
        return Ok(());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for chunk in items.chunks(items.len().div_ceil(WORKERS)) {
            let tx = tx.clone();
            let work = &work;
            scope.spawn(move || {
                for item in chunk {
                    if tx.send(work(item)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        for result in rx {
            if !send(result) {
                return Err(());
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(path: &str, git_dir: &str, common_dir: &str) -> Repo {
        Repo {
            path: path.into(),
            git_dir: git_dir.into(),
            common_dir: common_dir.into(),
        }
    }

    #[test]
    fn new_worktrees_stay_hidden_behind_their_listed_repository() {
        let main = repo("/c/a", "/c/a/.git", "/c/a/.git");
        let mut list = List {
            all: vec![main.clone()],
            listed: vec![main],
            gone: HashSet::new(),
        };
        let worktree = repo("/c/a-fix", "/c/a/.git/worktrees/a-fix", "/c/a/.git");
        assert!(list.add(worktree.clone(), false).is_none());
        assert!(list.add(worktree, false).is_none());
        assert_eq!(list.all.len(), 2);
        let other = repo("/c/b-wt", "/x/b/.git/worktrees/b-wt", "/x/b/.git");
        assert!(matches!(list.add(other, false), Some(Change::Added(1))));
        let shown = repo("/c/a-two", "/c/a/.git/worktrees/a-two", "/c/a/.git");
        assert!(matches!(list.add(shown, true), Some(Change::Added(2))));
    }

    #[test]
    fn only_stamp_files_mark_a_repository() {
        let watches = Watches {
            watcher: None,
            folders: HashMap::from([
                (PathBuf::from("/r/.git"), (true, vec![0])),
                (PathBuf::from("/r/.git/logs/refs/heads"), (false, vec![0])),
            ]),
            polled: HashSet::new(),
            trees: HashMap::new(),
            complete: false,
        };
        for (path, owners) in [
            ("/r/.git/HEAD", vec![0]),
            ("/r/.git/packed-refs", vec![0]),
            ("/r/.git/logs", vec![0]),
            ("/r/.git/index", vec![]),
            ("/r/.git/index.lock", vec![]),
            ("/r/.git/logs/refs/heads/main", vec![0]),
            ("/r/.git/logs/refs/heads/feature/x", vec![0]),
            ("/elsewhere/HEAD", vec![]),
        ] {
            assert_eq!(watches.owners(Path::new(path)), owners, "{path}");
        }
    }
}
