# gittop

`gittop` is a terminal monitor for your latest Git activity across every repository under a folder, including local-only repositories with no remote. It lists the repositories with commits in a time window, newest first, and updates within a fraction of a second of each new commit, merge, rebase, or pull. A feed view lists the commits themselves across all repositories.

It reads only your local repositories and never contacts a remote.

## Install

`gittop` needs `git` on the PATH, and building it needs Rust 1.88 or newer. Install it from [crates.io](https://crates.io/crates/gittop):

```sh
cargo install --locked gittop
```

`--locked` builds with the dependency versions `gittop` was tested with. From a checkout, use `cargo install --locked --path .`.

It has been tested on Linux.

## Run

```sh
gittop
gittop ~/code ~/work --since 7d
gittop --once --mine
gittop --once --feed --json
```

The default root is the current folder, searched three folder levels deep, with a 24-hour window. The live view adds repositories created under the root while it runs, such as a new clone, and drops removed ones, within a fraction of a second. `--once` prints the current activity and exits, and `--json` makes that output machine-readable.

A linked worktree (made with `git worktree add`) shares its repository's branches, so it would list the same commits as the main working tree. `gittop` lists only the main working tree, or the first worktree found when the main one is outside the searched folders. `--worktrees` lists each worktree as its own row.

## Columns

- `AGE`: time since the newest commit on any local branch.
- `REPOSITORY`: path relative to its root folder.
- `BRANCH`: the checked-out branch, shown in wide terminals.
- `COMMITS`: commits on local branches within the window.
- `UNPUSHED`: commits on local branches that no remote-tracking branch contains, `local` when the repository has no remote, or `-` when its branches have not changed within the window, so `git` was not run there.
- `LATEST COMMIT`: the newest commit's subject, with its author when someone else wrote it.

A commit counts as yours when its author name or email matches the repository's `user.name` or `user.email`, or a value passed with `--me`.

## Keys

- `q`, `Esc`: quit, or close the open pane.
- `?`: show or close help.
- `Tab`, `Shift-Tab`: sort by recency, commit count, unpushed count, or name.
- `f`: switch between repositories and the commit feed. The feed lists a commit shared by several clones once, naming each repository that has it.
- `m`: count only your own commits.
- `a`: also list repositories with no commits in the window.
- `w`, `W`: widen or narrow the window through 1h, 6h, 1d, 3d, 7d, and 30d.
- `/`: filter by repository, branch, author, or commit subject. `Enter` applies, `Esc` clears, `Ctrl-u` empties.
- `Enter`: show the selected repository's commits in the window.
- `r`: re-read the selected repository now.
- `p`, `Space`: pause or resume updates.
- Arrows, `j`/`k`, `PageUp`/`PageDown`, `Home`/`End`: move the selection.

The view, sort, and the `m` and `a` toggles are saved on exit to `$XDG_CONFIG_HOME/gittop/view.json`, or `~/.config/gittop/view.json`. `--no-config` ignores and keeps them for one session.

## Speed

Finding the repositories and reading their activity are both kept off the slow paths:

- The repository list is cached in `$XDG_CACHE_HOME/gittop/` (or `~/.cache/gittop/`), in one file per set of root folders and depth. The live view starts from the cached list, then searches the root folders again in the background and watches every searched folder, so the list catches up with repositories created or removed while `gittop` was not running and follows those created or removed while it runs. It rewrites the cache whenever the list changes. `--once` uses the cache only while no root folder's modification time has changed and it is less than a day old, and otherwise rebuilds it; `--rescan` forces a rebuild.
- Git updates a repository's `logs/HEAD`, its branch logs under `logs/refs/heads/`, `HEAD`, or `packed-refs` whenever a commit, merge, rebase, pull, or checkout happens. `gittop` asks the operating system to report changes in the folders holding those files, through [notify](https://github.com/notify-rs/notify) ([inotify](https://man7.org/linux/man-pages/man7/inotify.7.html) on Linux), then compares the files' newest modification time with the last one seen and runs `git` only when it moved. While no repository changes, it does no work beyond redrawing the ages once a second. A repository whose files are older than the window cannot have local commits in it, so `git` never runs there. Repositories whose folders cannot be watched, for example past the system's watch limit, are checked every `--interval` seconds (default 2) instead.
- The index is ignored because `git status` and similar commands rewrite it to refresh cached file metadata, unless run with `GIT_OPTIONAL_LOCKS=0`, without changing any history. Changes to it and to other files in the git directory wake `gittop` without running `git`.
- Uncommitted changes are not shown, because measuring them needs `git status` in every repository.

On a machine with 921 repositories under `/code`, a 24-hour `--once` run takes about 0.2 seconds with the repository list cached, and about 0.7 seconds when the list is rebuilt, since finding repositories reads their `.git` entries without running `git`. The live view watches about 36,000 folders, about 2,900 of them for commits and the rest for new repositories, picks up a commit or a new repository within about 0.4 seconds, and uses about 0.3% of one CPU core while idle. The system's limit on watched folders, `fs.inotify.max_user_watches`, is shared by every program a user runs; past it, unwatched repositories are checked every `--interval` seconds and new ones appear at the next start.

Commits that reach a repository only through `git fetch` update remote-tracking branches, not local ones, so they appear once they are merged or pulled into a local branch.

## Development

```sh
cargo test
cargo clippy --release --all-targets
```

## License

MIT
