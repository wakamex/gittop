mod activity;
mod app;
mod collector;
mod repos;
mod ui;

use app::{App, Prefs, View, age, parse_window};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Live view of your latest Git activity across every repository under a folder.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Folders to search for repositories.
    #[arg(default_value = "/code")]
    roots: Vec<PathBuf>,
    /// How many folder levels below each root to search.
    #[arg(long, default_value_t = 3)]
    depth: usize,
    /// Time window for listed activity, such as 30m, 24h, 7d, or 2w.
    #[arg(long, default_value = "24h", value_parser = parse_window)]
    since: u64,
    /// Extra author names or emails to count as yours, besides each repository's user.name
    /// and user.email.
    #[arg(long)]
    me: Vec<String>,
    /// Count only your own commits.
    #[arg(long)]
    mine: bool,
    /// Print the current activity once and exit.
    #[arg(long)]
    once: bool,
    /// With --once, print JSON.
    #[arg(long, requires = "once")]
    json: bool,
    /// With --once, list commits newest first instead of repositories.
    #[arg(long, requires = "once")]
    feed: bool,
    /// Seconds between checks for new activity.
    #[arg(long, default_value_t = 2)]
    interval: u64,
    /// Rebuild the cached repository list.
    #[arg(long)]
    rescan: bool,
    /// Ignore and do not save view preferences.
    #[arg(long)]
    no_config: bool,
}

/// Commits read per repository per window.
const COMMIT_LIMIT: usize = 200;
const WINDOWS: &[u64] = &[3600, 6 * 3600, 86400, 3 * 86400, 7 * 86400, 30 * 86400];

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let roots: Vec<PathBuf> = args
        .roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
    let repos = repos::load(&roots, args.depth, args.rescan);
    if args.once {
        return once(&args, &roots, repos);
    }
    let mut prefs = if args.no_config {
        Prefs::default()
    } else {
        load_prefs()
    };
    if args.mine {
        prefs.mine_only = true;
    }
    let mut app = App::new(repos.clone(), &roots, prefs, args.since);
    let (update_tx, update_rx) = mpsc::channel();
    let (request_tx, request_rx) = mpsc::channel();
    let settings = collector::Settings {
        window_secs: args.since,
        me: args.me.clone(),
        limit: COMMIT_LIMIT,
        interval: Duration::from_secs(args.interval.max(1)),
    };
    std::thread::Builder::new()
        .name("gittop-collector".into())
        .spawn(move || collector::run(repos, settings, update_tx, request_rx))?;

    let mut terminal = ratatui::init();
    let mut state = TableState::default().with_selected(Some(0));
    let result = (|| -> color_eyre::Result<()> {
        loop {
            while let Ok(update) = update_rx.try_recv() {
                match update {
                    collector::Update::Activity(i, a) => app.set_activity(i, a),
                    collector::Update::Ready => app.ready = true,
                }
            }
            let listed = match app.prefs.view {
                View::Repos => app.rows().len(),
                View::Feed => app.feed().len(),
            };
            app.selected = app.selected.min(listed.saturating_sub(1));
            state.select(Some(app.selected));
            terminal.draw(|frame| ui::draw(frame, &app, &mut state))?;
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !handle_key(&mut app, key, listed, &request_tx)
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    if !args.no_config {
        save_prefs(&app.prefs);
    }
    result
}

/// Applies one key press. Returns false to quit.
fn handle_key(
    app: &mut App,
    key: KeyEvent,
    listed: usize,
    requests: &mpsc::Sender<collector::Request>,
) -> bool {
    if app.editing_filter {
        match key.code {
            KeyCode::Enter => app.editing_filter = false,
            KeyCode::Esc => {
                app.editing_filter = false;
                app.filter.clear();
            }
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.filter.clear()
            }
            KeyCode::Char(c) => app.filter.push(c),
            _ => {}
        }
        app.selected = 0;
        return true;
    }
    if app.help {
        if matches!(key.code, KeyCode::Char('?' | 'q' | 'h') | KeyCode::Esc) {
            app.help = false;
        }
        return true;
    }
    if app.detail.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => app.detail = None,
            KeyCode::Down | KeyCode::Char('j') => app.detail_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => app.detail_scroll += 10,
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
            KeyCode::Char('r') => {
                let _ = requests.send(collector::Request::Refresh(app.detail.unwrap_or(0)));
            }
            _ => {}
        }
        return true;
    }
    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
    match key.code {
        _ if ctrl_c => return false,
        KeyCode::Char('q') => return false,
        KeyCode::Esc => {
            if app.filter.is_empty() {
                return false;
            }
            app.filter.clear();
        }
        KeyCode::Char('?' | 'h') => app.help = true,
        KeyCode::Char('/') => app.editing_filter = true,
        KeyCode::Tab => app.cycle_sort(true),
        KeyCode::BackTab => app.cycle_sort(false),
        KeyCode::Char('f') => {
            app.prefs.view = if app.prefs.view == View::Repos {
                View::Feed
            } else {
                View::Repos
            };
            app.selected = 0;
        }
        KeyCode::Char('m') => app.prefs.mine_only = !app.prefs.mine_only,
        KeyCode::Char('a') => app.prefs.show_all = !app.prefs.show_all,
        KeyCode::Char(c @ ('w' | 'W')) => {
            let position = WINDOWS
                .iter()
                .position(|&w| w >= app.window_secs)
                .unwrap_or(WINDOWS.len() - 1);
            let next = if c == 'w' {
                (position + 1).min(WINDOWS.len() - 1)
            } else {
                position.saturating_sub(1)
            };
            app.window_secs = WINDOWS[next];
            let _ = requests.send(collector::Request::Window(app.window_secs));
        }
        KeyCode::Char('p' | ' ') => {
            app.paused = !app.paused;
            let _ = requests.send(collector::Request::Pause(app.paused));
        }
        KeyCode::Char('r') | KeyCode::Enter => {
            if let Some(index) = selected_repo(app) {
                let _ = requests.send(collector::Request::Refresh(index));
                if key.code == KeyCode::Enter {
                    app.detail = Some(index);
                    app.detail_scroll = 0;
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected = (app.selected + 1).min(listed.saturating_sub(1))
        }
        KeyCode::Up | KeyCode::Char('k') => app.selected = app.selected.saturating_sub(1),
        KeyCode::PageDown => app.selected = (app.selected + 20).min(listed.saturating_sub(1)),
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(20),
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = listed.saturating_sub(1),
        _ => {}
    }
    true
}

/// The repository behind the selected row; in the feed, the first repository holding the commit.
fn selected_repo(app: &App) -> Option<usize> {
    match app.prefs.view {
        View::Repos => app.rows().get(app.selected).map(|r| r.index),
        View::Feed => {
            let item = app.feed().into_iter().nth(app.selected)?;
            app.names.iter().position(|n| *n == item.repos[0])
        }
    }
}

fn load_prefs() -> Prefs {
    std::fs::read(app::prefs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_prefs(prefs: &Prefs) {
    let path = app::prefs_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(prefs) {
        let _ = std::fs::write(path, bytes);
    }
}

fn once(args: &Args, roots: &[PathBuf], repos: Vec<repos::Repo>) -> color_eyre::Result<()> {
    let since = repos::now().saturating_sub(args.since);
    let read: Vec<activity::Activity> = std::thread::scope(|scope| {
        let chunks: Vec<_> = repos.chunks(repos.len().div_ceil(16).max(1)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|r| activity::read(r, since, &args.me, COMMIT_LIMIT))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let prefs = Prefs {
        mine_only: args.mine,
        ..Prefs::default()
    };
    let mut app = App::new(repos, roots, prefs, args.since);
    for (i, a) in read.into_iter().enumerate() {
        app.set_activity(i, a);
    }
    let rows = app.rows();
    if args.feed {
        let feed = app.feed();
        if args.json {
            let items: Vec<_> = feed
                .iter()
                .map(|f| serde_json::json!({"repositories": f.repos, "commit": f.commit}))
                .collect();
            println!("{}", serde_json::to_string_pretty(&items)?);
        } else {
            for item in feed {
                println!(
                    "{:>4}  {:<28}  {:<16}  {}",
                    age(item.commit.time),
                    item.repos.join(","),
                    item.commit.author,
                    item.commit.subject
                );
            }
        }
        return Ok(());
    }
    if args.json {
        let items: Vec<_> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "repository": row.name,
                    "path": app.repos[row.index].path,
                    "last_commit": row.last,
                    "commits": row.commits,
                    "mine": row.mine,
                    "activity": app.activity[row.index],
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    for row in &rows {
        let a = app.activity[row.index].as_ref();
        let unpushed = a.map_or("?".into(), |a| match (a.idle, a.has_remote) {
            (true, _) => "-".into(),
            (false, true) => a.unpushed.to_string(),
            (false, false) => "local".into(),
        });
        let latest = row.latest.as_ref().map_or("", |c| c.subject.as_str());
        println!(
            "{:>4}  {:<32}  {:>3} commits  {:>5} unpushed  {}",
            age(row.last),
            row.name,
            row.commits,
            unpushed,
            latest
        );
    }
    Ok(())
}
