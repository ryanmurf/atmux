use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, LaunchStage, Launcher, Mode},
    status::AgentStatus,
    tmux::Session,
};

const CYAN: Color = Color::Rgb(72, 202, 228);
const GREEN: Color = Color::Rgb(80, 200, 120);
const AMBER: Color = Color::Rgb(245, 183, 72);
const MUTED: Color = Color::Rgb(110, 118, 129);
const PANEL: Color = Color::Rgb(50, 58, 70);

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .areas(area);

    render_header(frame, app, header);
    let sidebar_width = sidebar_width(app, body.width);
    let [sessions, detail] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(24)])
            .spacing(1)
            .areas(body);
    app.session_area = sessions;
    render_sessions(frame, app, sessions);
    render_detail(frame, app, detail);
    render_footer(frame, app, footer);

    match app.mode {
        Mode::Help => render_help(frame, app),
        Mode::ConfirmKill => render_confirmation(frame, app),
        Mode::Launch => render_launcher(frame, app),
        Mode::Normal | Mode::Filter => {}
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (working, waiting) = app.status_counts();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" atmux ", Style::default().fg(Color::Black).bg(CYAN).bold()),
        Span::raw("  tmux agent control plane"),
        Span::raw("   "),
        Span::styled("●", Style::default().fg(GREEN)),
        Span::raw(format!(" {working} working   ")),
        Span::styled("◆", Style::default().fg(AMBER)),
        Span::raw(format!(" {waiting} waiting")),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(PANEL)),
    )
    .alignment(Alignment::Left);
    frame.render_widget(header, area);
}

fn render_sessions(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.filtered_indices();
    let items = visible
        .iter()
        .map(|&index| {
            let session = &app.sessions[index];
            let color = status_color(session.status);
            let suffix = session_suffix(app, session);
            let name_width = usize::from(area.width).saturating_sub(6 + suffix.width());
            let name = ellipsize(&session.name, name_width);
            let mut spans = vec![
                Span::styled(session.status.icon(), Style::default().fg(color).bold()),
                Span::raw(" "),
                Span::styled(name, Style::default().fg(Color::White)),
            ];
            if session.attached {
                spans.push(Span::styled("  attached", Style::default().fg(MUTED)));
            }
            if app.current_session.as_deref() == Some(&session.name) {
                spans.push(Span::styled("  atmux", Style::default().fg(CYAN)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();

    let title = if app.filter.is_empty() {
        format!(" Sessions ({}) ", visible.len())
    } else {
        format!(" Sessions · /{} ({}) ", app.filter, visible.len())
    };
    let list = List::new(items)
        .block(panel(title))
        .highlight_symbol("▌ ")
        .highlight_style(Style::default().bg(Color::Rgb(28, 38, 48)).bold());
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(session) = app.selected_session() else {
        let empty = Paragraph::new(Text::from(vec![
            Line::from("No tmux sessions found."),
            Line::from(""),
            Line::from(vec![
                Span::styled("n", Style::default().fg(CYAN).bold()),
                Span::raw(" launches your first agent."),
            ]),
        ]))
        .alignment(Alignment::Center)
        .block(panel(" Agent "));
        frame.render_widget(empty, area);
        return;
    };

    let [metadata, preview] = Layout::vertical([Constraint::Length(5), Constraint::Min(3)])
        .spacing(1)
        .areas(area);
    let status = session.status.label();
    let metadata_text = Text::from(vec![
        Line::from(vec![
            Span::styled(
                format!("{} {status}", session.status.icon()),
                Style::default().fg(status_color(session.status)).bold(),
            ),
            Span::styled(format!("  {}", session.agent), Style::default().fg(CYAN)),
            Span::styled(
                format!(
                    "  window {} · pane {} · pid {}",
                    session.window_index, session.pane_index, session.pane_pid
                ),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::styled("path  ", Style::default().fg(MUTED)),
            Span::raw(session.path.to_string_lossy()),
        ]),
        Line::from(vec![
            Span::styled("title ", Style::default().fg(MUTED)),
            Span::raw(if session.title.is_empty() {
                &session.command
            } else {
                &session.title
            }),
            Span::styled(
                format!("  · {} window(s)", session.windows),
                Style::default().fg(MUTED),
            ),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(metadata_text).block(panel(format!(" {} ", session.name))),
        metadata,
    );

    let lines = session.content.lines().count();
    let viewport = usize::from(preview.height.saturating_sub(2)).max(1);
    let bottom = lines.saturating_sub(viewport);
    let start = bottom.saturating_sub(usize::from(app.preview_scroll));
    let start = u16::try_from(start).unwrap_or(u16::MAX);
    let preview_title = if app.preview_scroll == 0 {
        " Live pane ".to_owned()
    } else {
        format!(" Live pane · {} lines back ", app.preview_scroll)
    };
    let preview_widget = Paragraph::new(session.content.as_str())
        .block(panel(preview_title))
        .scroll((start, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(preview_widget, preview);
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let line = if app.mode == Mode::Filter {
        Line::from(vec![
            Span::styled(" / ", Style::default().fg(Color::Black).bg(CYAN).bold()),
            Span::raw(format!(" {}", app.filter)),
            Span::styled("█", Style::default().fg(CYAN)),
            Span::styled("   enter accept · esc clear", Style::default().fg(MUTED)),
        ])
    } else if let Some((message, is_error)) = &app.message {
        Line::from(vec![
            Span::styled(
                if *is_error { " ! " } else { " ✓ " },
                Style::default()
                    .fg(Color::Black)
                    .bg(if *is_error { Color::Red } else { GREEN })
                    .bold(),
            ),
            Span::raw(format!(" {message}")),
        ])
    } else {
        shortcuts(&[
            ("e", "quick edit"),
            ("enter", "switch"),
            ("n", "new"),
            ("/", "filter"),
            ("x", "kill"),
            ("pgup", "preview"),
            ("?", "help"),
            ("q", "quit"),
        ])
    };
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_help(frame: &mut Frame<'_>, app: &App) {
    let area = centered(frame.area(), 62, 72);
    frame.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::styled("Keyboard", Style::default().fg(CYAN).bold()),
        Line::from(""),
        help_line("j / k, arrows", "move through sessions"),
        help_line("e", "quick edit in a popup; prefix + d returns"),
        help_line("enter / s", "full switch; prefix + L returns to atmux"),
        help_line("n", "launch an agent"),
        help_line("/", "filter sessions"),
        help_line("r", "refresh now"),
        help_line("page up/down", "scroll the pane preview"),
        help_line("x", "kill the selected session (with confirmation)"),
        help_line("q", "quit"),
        Line::from(""),
        Line::styled("Agent states", Style::default().fg(CYAN).bold()),
        Line::from(""),
        Line::from(vec![
            Span::styled("● working", Style::default().fg(GREEN)),
            Span::raw("   pane output or native busy indicator is active"),
        ]),
        Line::from(vec![
            Span::styled("◆ waiting", Style::default().fg(AMBER)),
            Span::raw("   agent prompt or approval needs attention"),
        ]),
        Line::from(vec![
            Span::styled("○ other", Style::default().fg(MUTED)),
            Span::raw("     no Codex or Claude process detected"),
        ]),
        Line::from(""),
        Line::styled(
            format!("Config: {}", app.config_path.display()),
            Style::default().fg(MUTED),
        ),
        Line::from(""),
        Line::styled("Press any key to close", Style::default().fg(AMBER)),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(modal_block(" Help "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_confirmation(frame: &mut Frame<'_>, app: &App) {
    let area = centered(frame.area(), 48, 24);
    frame.render_widget(Clear, area);
    let name = app
        .selected_session()
        .map_or("this session", |session| session.name.as_str());
    let text = Text::from(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("Kill tmux session "),
            Span::styled(name, Style::default().fg(AMBER).bold()),
            Span::raw("?"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("y", Style::default().fg(Color::Red).bold()),
            Span::raw(" confirm   "),
            Span::styled("any other key", Style::default().fg(CYAN)),
            Span::raw(" cancel"),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(modal_block(" Confirm ")),
        area,
    );
}

fn render_launcher(frame: &mut Frame<'_>, app: &App) {
    let Some(launcher) = &app.launcher else {
        return;
    };
    let area = centered(frame.area(), 76, 76);
    frame.render_widget(Clear, area);
    let inner = area.inner(Margin::new(2, 1));
    frame.render_widget(modal_block(" Launch agent "), area);
    let [steps, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(inner);

    render_launcher_steps(frame, launcher, steps);
    render_launcher_stage(frame, launcher, body);
    frame.render_widget(
        Paragraph::new(shortcuts(&[
            ("↑↓", "choose"),
            ("enter", "next"),
            ("esc", "back"),
        ]))
        .alignment(Alignment::Center),
        footer,
    );
}

fn render_launcher_steps(frame: &mut Frame<'_>, launcher: &Launcher, area: Rect) {
    let labels = [
        (LaunchStage::Directory, "1 directory"),
        (LaunchStage::Harness, "2 harness"),
        (LaunchStage::Profile, "3 profile"),
        (LaunchStage::Mode, "4 mode"),
        (LaunchStage::Name, "5 name"),
    ];
    let mut spans = Vec::new();
    for (index, (stage, label)) in labels.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ›  ", Style::default().fg(PANEL)));
        }
        spans.push(Span::styled(
            label,
            if launcher.stage == stage {
                Style::default().fg(Color::Black).bg(CYAN).bold()
            } else {
                Style::default().fg(MUTED)
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_launcher_stage(frame: &mut Frame<'_>, launcher: &Launcher, area: Rect) {
    match launcher.stage {
        LaunchStage::Directory => {
            let paths = launcher.filtered_directories();
            let items = paths
                .iter()
                .map(|path| ListItem::new(path.to_string_lossy().into_owned()))
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(
                launcher
                    .directory_selected
                    .min(items.len().saturating_sub(1)),
            ));
            let list = List::new(items)
                .block(panel(format!(
                    " Find folder · {} ",
                    launcher.directory_query
                )))
                .highlight_symbol("▌ ")
                .highlight_style(Style::default().fg(CYAN).bold());
            frame.render_stateful_widget(list, area, &mut state);
        }
        LaunchStage::Harness => {
            let harnesses = launcher
                .harnesses
                .iter()
                .map(|harness| title_case(harness))
                .collect::<Vec<_>>();
            render_choice_list(
                frame,
                area,
                " Agent harness ",
                &harnesses,
                launcher.harness_selected,
            );
        }
        LaunchStage::Profile => {
            let names = launcher
                .profiles
                .iter()
                .map(|profile| {
                    format!(
                        "{}  ·  {} {}",
                        profile.name,
                        profile.command,
                        profile.args.join(" ")
                    )
                })
                .collect::<Vec<_>>();
            render_choice_list(
                frame,
                area,
                " Launcher profile ",
                &names,
                launcher.profile_selected,
            );
        }
        LaunchStage::Mode => {
            let modes = launcher
                .modes
                .iter()
                .map(crate::config::ProfileMode::display_label)
                .collect::<Vec<_>>();
            render_choice_list(frame, area, " Model mode ", &modes, launcher.mode_selected);
        }
        LaunchStage::Name => {
            render_launch_name(frame, launcher, area);
        }
    }
}

fn render_launch_name(frame: &mut Frame<'_>, launcher: &Launcher, area: Rect) {
    let summary = vec![
        Line::from(vec![
            Span::styled("folder   ", Style::default().fg(MUTED)),
            Span::raw(
                launcher
                    .directory
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("harness  ", Style::default().fg(MUTED)),
            Span::raw(
                launcher
                    .harnesses
                    .get(launcher.harness_selected)
                    .map_or("", String::as_str),
            ),
        ]),
        Line::from(vec![
            Span::styled("profile  ", Style::default().fg(MUTED)),
            Span::raw(
                launcher
                    .profiles
                    .get(launcher.profile_selected)
                    .map_or("", |profile| profile.name.as_str()),
            ),
        ]),
        Line::from(vec![
            Span::styled("mode     ", Style::default().fg(MUTED)),
            Span::raw(
                launcher
                    .modes
                    .get(launcher.mode_selected)
                    .map(crate::config::ProfileMode::display_label)
                    .unwrap_or_default(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("session  ", Style::default().fg(CYAN).bold()),
            Span::styled(&launcher.name, Style::default().fg(Color::White).bold()),
            Span::styled("█", Style::default().fg(CYAN)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(summary)
            .block(panel(" Review and name "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_choice_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    choices: &[String],
    selected: usize,
) {
    let items = choices
        .iter()
        .map(|choice| ListItem::new(choice.as_str()))
        .collect::<Vec<_>>();
    let mut state = ListState::default()
        .with_selected((!items.is_empty()).then_some(selected.min(items.len().saturating_sub(1))));
    let list = List::new(items)
        .block(panel(title))
        .highlight_symbol("▌ ")
        .highlight_style(Style::default().fg(CYAN).bold());
    frame.render_stateful_widget(list, area, &mut state);
}

fn shortcuts(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, action)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", Style::default().fg(PANEL)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(CYAN).bold(),
        ));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn help_line(key: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<18}"), Style::default().fg(CYAN).bold()),
        Span::raw(action),
    ])
}

fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Working => GREEN,
        AgentStatus::Waiting => AMBER,
        AgentStatus::Other => MUTED,
    }
}

fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL))
}

fn modal_block(title: &'static str) -> Block<'static> {
    Block::default()
        .title(Span::styled(title, Style::default().fg(CYAN).bold()))
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(CYAN))
        .style(Style::default().bg(Color::Rgb(12, 17, 23)))
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let [vertical] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(height_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [horizontal] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(width_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    horizontal
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn sidebar_width(app: &App, body_width: u16) -> u16 {
    let visible = app.filtered_indices();
    let title = if app.filter.is_empty() {
        format!(" Sessions ({}) ", visible.len())
    } else {
        format!(" Sessions · /{} ({}) ", app.filter, visible.len())
    };
    let title_width = title.width() + 2;
    let row_width = visible
        .iter()
        .map(|&index| {
            let session = &app.sessions[index];
            6 + session.name.width() + session_suffix(app, session).width()
        })
        .max()
        .unwrap_or_default();
    cap_sidebar_width(title_width.max(row_width), body_width)
}

fn cap_sidebar_width(desired: usize, body_width: u16) -> u16 {
    let quarter = usize::from(body_width / 4).max(1);
    u16::try_from(desired.min(quarter).max(1)).unwrap_or(u16::MAX)
}

fn session_suffix(app: &App, session: &Session) -> String {
    let mut suffix = String::new();
    if session.attached {
        suffix.push_str("  attached");
    }
    if app.current_session.as_deref() == Some(&session.name) {
        suffix.push_str("  atmux");
    }
    suffix
}

fn ellipsize(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_owned();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let target = max_width - 3;
    let mut width = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = character.width().unwrap_or_default();
        if width + character_width > target {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_collapses_but_never_exceeds_one_quarter() {
        assert_eq!(cap_sidebar_width(18, 120), 18);
        assert_eq!(cap_sidebar_width(80, 120), 30);
    }

    #[test]
    fn long_names_are_ellipsized_to_the_exact_width() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("a-very-long-session", 10), "a-very-...");
        assert_eq!(ellipsize("session", 3), "...");
        assert_eq!(ellipsize("session", 0), "");
    }
}
