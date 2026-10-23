//! Background collection: waits for the files Git updates on each commit to change, and runs
//! git only in repositories whose change stamp moved, so the UI thread never waits on git and
//! nothing runs while no repository changes.

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
}

pub enum Update {
    Activity(usize, Activity),
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

pub fn run(
    repos: Vec<Repo>,
    settings: Settings,
    updates: Sender<Update>,
    requests: Receiver<Request>,
    sender: Sender<Request>,
) {
    let mut settings = settings;
    let mut watches = Watches::new(sender);
    let mut stamps: HashMap<usize, u64> = HashMap::new();
    let mut dirty: HashSet<usize> = (0..repos.len()).collect();
    let mut forced = HashSet::new();
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
            // Watch before reading the stamp, so a change between the two is never missed.
            for &i in &check {
                watches.add(i, &repos[i]);
            }
            let changed: Vec<usize> = check
                .into_iter()
                .filter(|&i| {
                    let stamp = repos[i].stamp();
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
                    activity::read(&repos[i], since, &settings.me, settings.limit),
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
            }
        }

        let first_request = if watches.polled.is_empty() {
            requests.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            requests.recv_timeout(next_poll.saturating_duration_since(Instant::now()))
        };
        let first_request = match first_request {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        std::thread::sleep(SETTLE);
        for request in std::iter::once(first_request).chain(requests.try_iter()) {
            match request {
                Request::Refresh(index) => {
                    forced.insert(index);
                }
                Request::Pause(value) => paused = value,
                Request::Window(secs) => {
                    settings.window_secs = secs;
                    stamps.clear();
                    dirty.extend(0..repos.len());
                }
                // Opening a file, as reading a stamp does, changes nothing.
                Request::Files(Ok(event)) if matches!(event.kind, EventKind::Access(_)) => {}
                Request::Files(Ok(event)) if !event.need_rescan() => {
                    for path in &event.paths {
                        dirty.extend(watches.owners(path));
                    }
                }
                Request::Files(_) => dirty.extend(0..repos.len()),
            }
        }
    }
}

/// Folder watches for each repository's change-stamp files. Repositories whose folders cannot
/// be watched, for example past the system's watch limit, are checked every interval instead.
struct Watches {
    watcher: Option<RecommendedWatcher>,
    /// Watched folder, whether it is a git directory, and the repositories it belongs to.
    folders: HashMap<PathBuf, (bool, Vec<usize>)>,
    polled: HashSet<usize>,
}

impl Watches {
    fn new(sender: Sender<Request>) -> Self {
        let watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(Request::Files(event));
        })
        .ok();
        Watches {
            watcher,
            folders: HashMap::new(),
            polled: HashSet::new(),
        }
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

    #[test]
    fn only_stamp_files_mark_a_repository() {
        let watches = Watches {
            watcher: None,
            folders: HashMap::from([
                (PathBuf::from("/r/.git"), (true, vec![0])),
                (PathBuf::from("/r/.git/logs/refs/heads"), (false, vec![0])),
            ]),
            polled: HashSet::new(),
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
