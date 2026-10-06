//! Reads a repository's recent commits, branch, and unpushed count with git.

use crate::repos::Repo;
use serde::Serialize;
use std::process::Command;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub time: u64,
    pub author: String,
    pub email: String,
    pub subject: String,
    /// True when the author matches one of the repository's identities or `--me`.
    pub mine: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Activity {
    pub branch: String,
    /// Commits on local branches since the window start, newest first.
    pub commits: Vec<Commit>,
    /// Commit time of the newest commit on any local branch, within the window or not.
    pub last_commit: u64,
    /// Commits on local branches that no remote-tracking branch contains.
    pub unpushed: usize,
    pub has_remote: bool,
    /// True when git was not run because nothing changed since the window started.
    pub idle: bool,
    /// Newest change to the repository's Git logs, HEAD, or packed refs.
    pub stamp: u64,
}

/// Field separator unlikely to appear in commit subjects.
const SEP: char = '\u{1f}';

fn git(repo: &Repo, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(&repo.path)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads a repository's activity. A repository whose Git logs, HEAD, and packed refs have not
/// changed since `since` cannot have new local commits in the window, so it is described from
/// those files alone without running git.
pub fn read(repo: &Repo, since: u64, me: &[String], limit: usize) -> Activity {
    let stamp = repo.stamp();
    if stamp < since {
        return Activity {
            branch: head_branch(repo),
            idle: true,
            stamp,
            ..Activity::default()
        };
    }
    let branch = git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|b| b.trim().to_string())
        .unwrap_or_else(|| "(detached)".to_string());
    let format = format!("--format=%H{SEP}%ct{SEP}%an{SEP}%ae{SEP}%s");
    let since_arg = format!("--since={since}");
    let limit_arg = format!("-n{limit}");
    let log = git(
        repo,
        &["log", "--branches", &since_arg, &limit_arg, &format],
    )
    .unwrap_or_default();
    let commits: Vec<Commit> = log
        .lines()
        .filter_map(|line| parse(line, repo, me))
        .collect();
    let last_commit = commits.first().map(|c| c.time).unwrap_or_else(|| {
        git(repo, &["log", "--branches", "-n1", "--format=%ct"])
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    });
    let has_remote = git(repo, &["remote"]).is_some_and(|r| !r.trim().is_empty());
    let unpushed = git(
        repo,
        &["rev-list", "--count", "--branches", "--not", "--remotes"],
    )
    .and_then(|n| n.trim().parse().ok())
    .unwrap_or(0);
    Activity {
        branch,
        commits,
        last_commit,
        unpushed,
        has_remote,
        idle: false,
        stamp,
    }
}

fn head_branch(repo: &Repo) -> String {
    let head = std::fs::read_to_string(repo.git_dir.join("HEAD")).unwrap_or_default();
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map_or_else(|| "(detached)".to_string(), str::to_string)
}

fn parse(line: &str, repo: &Repo, me: &[String]) -> Option<Commit> {
    let mut fields = line.splitn(5, SEP);
    let sha = fields.next()?.to_string();
    let time = fields.next()?.parse().ok()?;
    let author = fields.next()?.to_string();
    let email = fields.next()?.to_string();
    let subject = fields.next().unwrap_or_default().to_string();
    let mine = repo
        .identities
        .iter()
        .chain(me)
        .any(|id| id.eq_ignore_ascii_case(&author) || id.eq_ignore_ascii_case(&email));
    Some(Commit {
        sha,
        time,
        author,
        email,
        subject,
        mine,
    })
}
