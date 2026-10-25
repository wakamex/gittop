//! View state: which repositories are listed, in what order, and how keys change that.

use crate::activity::{Activity, Commit};
use crate::repos::{self, Repo};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    Repos,
    Feed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    Recent,
    Commits,
    Unpushed,
    Name,
}

impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Recent, Sort::Commits, Sort::Unpushed, Sort::Name];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "recent",
            Sort::Commits => "commits",
            Sort::Unpushed => "unpushed",
            Sort::Name => "name",
        }
    }
}

/// Settings remembered across sessions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prefs {
    pub view: View,
    pub sort: Sort,
    pub mine_only: bool,
    pub show_all: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            view: View::Repos,
            sort: Sort::Recent,
            mine_only: false,
            show_all: false,
        }
    }
}

pub fn prefs_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| repos::home().join(".config"))
        .join("gittop/view.json")
}

pub struct Row {
    pub index: usize,
    pub name: String,
    pub last: u64,
    pub commits: usize,
    pub mine: usize,
    pub latest: Option<Commit>,
}

pub struct FeedItem {
    pub commit: Commit,
    pub repos: Vec<String>,
}

pub struct App {
    pub repos: Vec<Repo>,
    pub names: Vec<String>,
    pub activity: Vec<Option<Activity>>,
    pub prefs: Prefs,
    pub window_secs: u64,
    pub filter: String,
    pub editing_filter: bool,
    pub selected: usize,
    pub detail: Option<usize>,
    pub detail_scroll: usize,
    pub help: bool,
    pub paused: bool,
    pub ready: bool,
    /// Repositories removed while running; they keep their index but are not listed.
    pub gone: HashSet<usize>,
}

impl App {
    pub fn new(repos: Vec<Repo>, roots: &[PathBuf], prefs: Prefs, window_secs: u64) -> Self {
        let names = repos.iter().map(|r| r.name(roots)).collect();
        let activity = vec![None; repos.len()];
        App {
            repos,
            names,
            activity,
            prefs,
            window_secs,
            filter: String::new(),
            editing_filter: false,
            selected: 0,
            detail: None,
            detail_scroll: 0,
            help: false,
            paused: false,
            ready: false,
            gone: HashSet::new(),
        }
    }

    /// Lists a repository found while running, at the next index.
    pub fn add(&mut self, repo: Repo, roots: &[PathBuf]) {
        self.names.push(repo.name(roots));
        self.repos.push(repo);
        self.activity.push(None);
    }

    pub fn remove(&mut self, index: usize) {
        self.gone.insert(index);
        self.activity[index] = None;
        if self.detail == Some(index) {
            self.detail = None;
        }
    }

    pub fn set_activity(&mut self, index: usize, activity: Activity) {
        self.activity[index] = Some(activity);
    }

    fn matches_filter(&self, index: usize) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let needle = self.filter.to_lowercase();
        let activity = self.activity[index].as_ref();
        self.names[index].to_lowercase().contains(&needle)
            || activity.is_some_and(|a| {
                a.branch.to_lowercase().contains(&needle)
                    || a.commits.iter().any(|c| {
                        c.subject.to_lowercase().contains(&needle)
                            || c.author.to_lowercase().contains(&needle)
                    })
            })
    }

    pub fn rows(&self) -> Vec<Row> {
        let window_start = repos::now().saturating_sub(self.window_secs);
        let mut rows: Vec<Row> = self
            .activity
            .iter()
            .enumerate()
            .filter_map(|(index, activity)| {
                let activity = activity.as_ref()?;
                let counted: Vec<&Commit> = activity
                    .commits
                    .iter()
                    .filter(|c| !self.prefs.mine_only || c.mine)
                    .collect();
                let latest = counted.first().map(|c| (*c).clone());
                let last = match (&latest, self.prefs.mine_only) {
                    (Some(commit), _) => commit.time,
                    (None, true) => 0,
                    (None, false) if activity.idle => activity.stamp,
                    (None, false) => activity.last_commit,
                };
                let active = last >= window_start;
                if !(active || self.prefs.show_all) || !self.matches_filter(index) {
                    return None;
                }
                Some(Row {
                    index,
                    name: self.names[index].clone(),
                    last,
                    commits: counted.len(),
                    mine: activity.commits.iter().filter(|c| c.mine).count(),
                    latest,
                })
            })
            .collect();
        let key = |row: &Row| {
            let activity = self.activity[row.index].as_ref();
            match self.prefs.sort {
                Sort::Recent => row.last,
                Sort::Commits => row.commits as u64,
                Sort::Unpushed => activity.map_or(0, |a| a.unpushed as u64),
                Sort::Name => 0,
            }
        };
        if self.prefs.sort == Sort::Name {
            rows.sort_by(|a, b| a.name.cmp(&b.name));
        } else {
            rows.sort_by(|a, b| {
                key(b)
                    .cmp(&key(a))
                    .then(b.last.cmp(&a.last))
                    .then(a.name.cmp(&b.name))
            });
        }
        rows
    }

    /// Commits in the window across all listed repositories, newest first, with each commit
    /// shown once and every repository that contains it.
    pub fn feed(&self) -> Vec<FeedItem> {
        let mut items: Vec<FeedItem> = Vec::new();
        let mut seen = std::collections::HashMap::new();
        for row in self.rows() {
            let Some(activity) = &self.activity[row.index] else {
                continue;
            };
            for commit in activity
                .commits
                .iter()
                .filter(|c| !self.prefs.mine_only || c.mine)
            {
                if let Some(&at) = seen.get(&commit.sha) {
                    let item: &mut FeedItem = &mut items[at];
                    item.repos.push(row.name.clone());
                } else {
                    seen.insert(commit.sha.clone(), items.len());
                    items.push(FeedItem {
                        commit: commit.clone(),
                        repos: vec![row.name.clone()],
                    });
                }
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.commit.time));
        items
    }

    pub fn cycle_sort(&mut self, forward: bool) {
        let position = Sort::ALL
            .iter()
            .position(|s| *s == self.prefs.sort)
            .unwrap_or(0);
        let next = if forward {
            position + 1
        } else {
            position + Sort::ALL.len() - 1
        };
        self.prefs.sort = Sort::ALL[next % Sort::ALL.len()];
    }
}

pub fn age(time: u64) -> String {
    if time == 0 {
        return "-".to_string();
    }
    let secs = repos::now().saturating_sub(time);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        86400..1_209_600 => format!("{}d", secs / 86400),
        1_209_600..31_536_000 => format!("{}w", secs / 604_800),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Parses durations such as `30m`, `24h`, `7d`, and `2w`.
pub fn parse_window(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration: {text}"))?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" | "" => 3600,
        "d" => 86400,
        "w" => 604_800,
        _ => return Err(format!("unknown unit in {text}; use s, m, h, d, or w")),
    };
    Ok(number * scale)
}

pub fn format_window(secs: u64) -> String {
    for (scale, unit) in [(604_800, "w"), (86400, "d"), (3600, "h"), (60, "m")] {
        if secs >= scale && secs.is_multiple_of(scale) {
            return format!("{}{unit}", secs / scale);
        }
    }
    format!("{secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_windows() {
        assert_eq!(parse_window("30m"), Ok(1800));
        assert_eq!(parse_window("24"), Ok(86400));
        assert_eq!(parse_window("2w"), Ok(1_209_600));
        assert!(parse_window("5y").is_err());
        assert_eq!(format_window(86400), "1d");
        assert_eq!(format_window(5400), "90m");
    }

    fn commit(sha: &str, time: u64, mine: bool) -> Commit {
        Commit {
            sha: sha.into(),
            time,
            author: "a".into(),
            email: "a@example.com".into(),
            subject: format!("subject {sha}"),
            mine,
        }
    }

    fn app_with(activity: Vec<Vec<Commit>>) -> App {
        let repos: Vec<Repo> = (0..activity.len())
            .map(|i| Repo {
                path: PathBuf::from(format!("/r/{i}")),
                git_dir: PathBuf::new(),
                common_dir: PathBuf::new(),
            })
            .collect();
        let mut app = App::new(repos, &[PathBuf::from("/r")], Prefs::default(), 86400);
        for (i, commits) in activity.into_iter().enumerate() {
            let last_commit = commits.first().map_or(0, |c| c.time);
            app.set_activity(
                i,
                Activity {
                    commits,
                    last_commit,
                    ..Activity::default()
                },
            );
        }
        app
    }

    #[test]
    fn rows_hide_idle_repositories_and_sort_by_recency() {
        let now = repos::now();
        let app = app_with(vec![
            vec![commit("a1", now - 100, true)],
            vec![],
            vec![commit("c1", now - 10, false), commit("c2", now - 20, true)],
        ]);
        let names: Vec<String> = app.rows().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["2", "0"]);
    }

    #[test]
    fn mine_only_counts_and_dates_only_own_commits() {
        let now = repos::now();
        let mut app = app_with(vec![vec![
            commit("c1", now - 10, false),
            commit("c2", now - 20, true),
        ]]);
        app.prefs.mine_only = true;
        let row = &app.rows()[0];
        assert_eq!((row.commits, row.last), (1, now - 20));
    }

    #[test]
    fn feed_lists_a_commit_shared_by_clones_once() {
        let now = repos::now();
        let app = app_with(vec![
            vec![commit("x", now - 5, true)],
            vec![commit("x", now - 5, true)],
        ]);
        let feed = app.feed();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].repos.len(), 2);
    }
}
