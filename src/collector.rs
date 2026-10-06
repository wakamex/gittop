//! Background collection: stats every repository each tick and runs git only where
//! something changed, so the UI thread never waits on git.

use crate::activity::{self, Activity};
use crate::repos::{self, Repo};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

pub struct Settings {
    pub window_secs: u64,
    pub me: Vec<String>,
    pub limit: usize,
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
}

const WORKERS: usize = 16;

pub fn run(
    repos: Vec<Repo>,
    settings: Settings,
    updates: Sender<Update>,
    requests: Receiver<Request>,
) {
    let mut settings = settings;
    let mut stamps: HashMap<usize, u64> = HashMap::new();
    let mut paused = false;
    let mut first = true;
    loop {
        let started = Instant::now();
        let mut forced = Vec::new();
        while let Ok(request) = requests.try_recv() {
            match request {
                Request::Refresh(index) => forced.push(index),
                Request::Pause(value) => paused = value,
                Request::Window(secs) => {
                    settings.window_secs = secs;
                    stamps.clear();
                }
            }
        }
        if !paused || !forced.is_empty() {
            let changed: Vec<usize> = (0..repos.len())
                .filter(|&i| {
                    let stamp = repos[i].stamp();
                    let fresh = stamps.get(&i) != Some(&stamp);
                    if fresh && !paused {
                        stamps.insert(i, stamp);
                    }
                    (fresh && !paused) || forced.contains(&i)
                })
                .collect();
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
        std::thread::sleep(settings.interval.saturating_sub(started.elapsed()));
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
