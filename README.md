# gittop

`gittop` is a terminal monitor for your latest Git activity across every repository under a folder, including local-only repositories with no remote. It lists the repositories with commits in a time window, newest first, and updates within a couple of seconds of each new commit, merge, rebase, or pull. A feed view lists the commits themselves across all repositories.

It reads only your local repositories and never contacts a remote.

## Run

```sh
cargo run --release
cargo run --release -- /code ~/projects --since 7d
cargo run --release -- --once --mine
cargo run --release -- --once --feed --json
```

The default root is `/code`, searched three folder levels deep, with a 24-hour window. `--once` prints the current activity and exits, and `--json` makes that output machine-readable.

A linked worktree (made with `git worktree add`) shares its repository's branches, so it would list the same commits as the main working tree. `gittop` lists only the main working tree, or the first worktree found when the main one is outside the searched folders. `--worktrees` lists each worktree as its own row.

## Columns

- `AGE`: time since the newest commit on any local branch.
- `REPOSITORY`: path relative to its root folder.
- `BRANCH`: the checked-out branch, shown in wide terminals.
- `COMMITS`: commits on local branches within the window.
- `UNPUSHED`: commits on local branches that no remote-tracking branch contains, or `local` when the repository has no remote.
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

- The repository list is cached in `$XDG_CACHE_HOME/gittop/repos.json` (or `~/.cache/gittop/repos.json`). It is rebuilt when a root folder's modification time changes, which happens when a project folder is added or removed, after a day, or with `--rescan`. Each run drops cached repositories whose git directory no longer exists, so removing a nested repository takes effect at once, while a new nested repository appears with the next rebuild.
- Git updates a repository's `logs/HEAD`, its branch logs under `logs/refs/heads/`, `HEAD`, or `packed-refs` whenever a commit, merge, rebase, pull, or checkout happens. `gittop` checks those files' modification times every `--interval` seconds (default 2), which takes milliseconds for hundreds of repositories, and runs `git` only in repositories where one changed. A repository whose files are older than the window cannot have local commits in it, so `git` never runs there. The index is ignored because `git status` and similar commands rewrite it to refresh cached file metadata, unless run with `GIT_OPTIONAL_LOCKS=0`, without changing any history.
- Uncommitted changes are not shown, because measuring them needs `git status` in every repository.

On a machine with 938 repositories under `/code`, a 24-hour `--once` run takes about 0.1 seconds with the repository list cached.

Commits that reach a repository only through `git fetch` update remote-tracking branches, not local ones, so they appear once they are merged or pulled into a local branch.

## Development

```sh
cargo test
cargo clippy --release
```

## License

MIT
