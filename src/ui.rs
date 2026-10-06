//! Rendering for the repository table, the commit feed, the detail pane, and help.

use crate::app::{App, View, age, format_window};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};

const HELP: &[(&str, &str)] = &[
    ("q, Esc", "quit, or close the open pane"),
    ("?", "show or close this help"),
    (
        "Tab, Shift-Tab",
        "sort by recent, commits, unpushed, or name",
    ),
    ("f", "switch between repositories and the commit feed"),
    ("m", "count only your own commits"),
    ("a", "also list repositories with no commits in the window"),
    ("w / W", "widen or narrow the time window"),
    (
        "/",
        "filter by repository, branch, author, or subject; Enter applies, Esc clears",
    ),
    ("Enter", "show the selected repository's commits"),
    ("r", "re-read the selected repository now"),
    ("p, Space", "pause or resume collection"),
    (
        "Up/Down, j/k, PageUp/PageDown, Home/End",
        "move the selection",
    ),
];

pub fn draw(frame: &mut Frame, app: &App, state: &mut TableState) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let rows = app.rows();
    let listed = match app.prefs.view {
        View::Repos => rows.len(),
        View::Feed => app.feed().len(),
    };
    let status = format!(
        " gittop  {} of {} repos  window {}  sort {}  {}{}{}",
        rows.len(),
        app.repos.len(),
        format_window(app.window_secs),
        app.prefs.sort.label(),
        if app.prefs.mine_only {
            "mine"
        } else {
            "everyone"
        },
        if app.paused { "  PAUSED" } else { "" },
        if app.ready { "" } else { "  scanning..." },
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::new().add_modifier(Modifier::REVERSED)),
        header,
    );

    match app.prefs.view {
        View::Repos => draw_repos(frame, app, &rows, body, state),
        View::Feed => draw_feed(frame, app, body, state),
    }

    let hint = if app.editing_filter {
        format!(" filter: {}_", app.filter)
    } else if !app.filter.is_empty() {
        format!(
            " filter: {}  ({listed} shown)  / edit  ? help  q quit",
            app.filter
        )
    } else {
        " Tab sort  f feed  m mine  a all  w/W window  / filter  Enter details  ? help  q quit"
            .to_string()
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        footer,
    );

    if let Some(index) = app.detail {
        draw_detail(frame, app, index, body);
    }
    if app.help {
        draw_help(frame, body);
    }
}

fn count_style(value: usize) -> Style {
    if value == 0 {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::Yellow)
    }
}

fn draw_repos(
    frame: &mut Frame,
    app: &App,
    rows: &[crate::app::Row],
    area: Rect,
    state: &mut TableState,
) {
    let wide = area.width >= 100;
    let lines = rows.iter().map(|row| {
        let activity = app.activity[row.index].as_ref();
        let unpushed = match activity {
            Some(a) if a.idle => Span::styled("-", Style::new().fg(Color::DarkGray)),
            Some(a) if !a.has_remote => Span::styled("local", Style::new().fg(Color::Magenta)),
            Some(a) => Span::styled(a.unpushed.to_string(), count_style(a.unpushed)),
            None => Span::raw("?"),
        };
        let latest = row.latest.as_ref().map_or(String::new(), |c| {
            if c.mine {
                c.subject.clone()
            } else {
                format!("{} ({})", c.subject, c.author)
            }
        });
        let mut cells = vec![
            Span::styled(age(row.last), Style::new().fg(Color::Cyan)),
            Span::styled(row.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
        ];
        if wide {
            cells.push(Span::raw(
                activity.map_or(String::new(), |a| a.branch.clone()),
            ));
        }
        cells.extend([
            Span::styled(row.commits.to_string(), count_style(row.commits)),
            unpushed,
            Span::raw(latest),
        ]);
        Row::new(cells)
    });
    let name_width = rows
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 32) as u16;
    let mut widths = vec![Constraint::Length(4), Constraint::Length(name_width)];
    let mut titles = vec!["AGE", "REPOSITORY"];
    if wide {
        widths.push(Constraint::Length(16));
        titles.push("BRANCH");
    }
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Fill(1),
    ]);
    titles.extend(["COMMITS", "UNPUSHED", "LATEST COMMIT"]);
    let table = Table::new(lines, widths)
        .header(Row::new(titles).style(Style::new().add_modifier(Modifier::UNDERLINED)))
        .row_highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(table, area, state);
}

fn draw_feed(frame: &mut Frame, app: &App, area: Rect, state: &mut TableState) {
    let feed = app.feed();
    let lines = feed.iter().map(|item| {
        let repos = if item.repos.len() > 1 {
            format!("{} +{}", item.repos[0], item.repos.len() - 1)
        } else {
            item.repos[0].clone()
        };
        let author_style = if item.commit.mine {
            Style::new().fg(Color::Green)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Row::new(vec![
            Span::styled(age(item.commit.time), Style::new().fg(Color::Cyan)),
            Span::styled(repos, Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(item.commit.author.clone(), author_style),
            Span::raw(item.commit.subject.clone()),
        ])
    });
    let widths = [
        Constraint::Length(4),
        Constraint::Length(28),
        Constraint::Length(16),
        Constraint::Fill(1),
    ];
    let table = Table::new(lines, widths)
        .header(
            Row::new(["AGE", "REPOSITORY", "AUTHOR", "SUBJECT"])
                .style(Style::new().add_modifier(Modifier::UNDERLINED)),
        )
        .row_highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(table, area, state);
}

fn draw_detail(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let popup = inset(area, 2, 1);
    frame.render_widget(Clear, popup);
    let repo = &app.repos[index];
    let mut lines = vec![Line::from(repo.path.to_string_lossy().into_owned())];
    if let Some(a) = &app.activity[index] {
        let remote = if a.has_remote {
            format!("{} unpushed", a.unpushed)
        } else {
            "no remote".to_string()
        };
        lines.push(Line::from(format!(
            "branch {}  {remote}  last commit {} ago",
            a.branch,
            age(a.last_commit)
        )));
        lines.push(Line::from(""));
        if a.commits.is_empty() {
            lines.push(Line::from(format!(
                "No commits on local branches in the last {}.",
                format_window(app.window_secs)
            )));
        }
        for c in &a.commits {
            let author = if c.mine {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>4} ", age(c.time)), Style::new().fg(Color::Cyan)),
                Span::styled(
                    format!("{} ", &c.sha[..c.sha.len().min(8)]),
                    Style::new().fg(Color::Yellow),
                ),
                Span::styled(format!("{} ", c.author), author),
                Span::raw(c.subject.clone()),
            ]));
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} (Esc closes) ", app.names[index]));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.detail_scroll as u16, 0)),
        popup,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = inset(area, 4, 1);
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(keys, action)| {
            Line::from(vec![
                Span::styled(format!("{keys:<42}"), Style::new().fg(Color::Yellow)),
                Span::raw(*action),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" gittop keys (? closes) ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn inset(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x + x.min(area.width / 4),
        y: area.y + y.min(area.height / 4),
        width: area.width.saturating_sub(2 * x.min(area.width / 4)),
        height: area.height.saturating_sub(2 * y.min(area.height / 4)),
    }
}
