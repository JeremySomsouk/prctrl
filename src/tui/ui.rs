use crate::tui::app::{App, PrAction, Tab};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Wrap,
};
use ratatui::Frame;

const BACKGROUND: Color = Color::Rgb(18, 24, 35);
const PANEL: Color = Color::Rgb(25, 33, 47);
const TEXT: Color = Color::Rgb(232, 237, 245);
const MUTED: Color = Color::Rgb(168, 181, 201);
const ACCENT: Color = Color::Rgb(128, 218, 222);
const BORDER: Color = Color::Rgb(86, 105, 130);
const WARNING: Color = Color::Rgb(255, 209, 128);

#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn text(self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }
    fn panel(self) -> Style {
        if self.color {
            self.text(TEXT).bg(PANEL)
        } else {
            Style::default()
        }
    }
    fn selected(self) -> Style {
        if self.color {
            Style::default()
                .fg(BACKGROUND)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        }
    }
    fn block(self, title: impl Into<Line<'static>>) -> Block<'static> {
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.text(BORDER))
            .style(self.panel())
    }
}

/// Responsive review desk. Formatting work is bounded by the visible viewport.
pub struct Ui;

impl Ui {
    pub fn draw(frame: &mut Frame, app: &mut App) {
        let size = frame.area();
        let theme = Theme {
            color: app.use_color,
        };
        let background = if theme.color {
            theme.text(TEXT).bg(BACKGROUND)
        } else {
            Style::default()
        };
        frame.render_widget(Block::default().style(background), size);
        if size.width < 30 || size.height < 10 {
            frame.render_widget(
                Paragraph::new("PRCTRL\nResize to at least 30 x 10.\nq / Ctrl+C quit")
                    .style(theme.text(TEXT))
                    .wrap(Wrap { trim: false }),
                size,
            );
            return;
        }
        let compact = size.width < 100;
        let search_height = if app.filter_editing || !app.filter.is_empty() {
            3
        } else {
            0
        };
        let message_height = if app.error.is_some() || app.info.is_some() {
            3
        } else {
            0
        };
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(if compact { 1 } else { 0 }),
                Constraint::Min(0),
                Constraint::Length(search_height),
                Constraint::Length(message_height),
                Constraint::Length(2),
            ])
            .split(size);
        Self::brand(frame, app, areas[0], theme);
        if compact {
            Self::tabs(frame, app, areas[1], theme);
        }
        if compact {
            Self::content(frame, app, areas[2], theme);
        } else {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(22), Constraint::Min(0)])
                .split(areas[2]);
            Self::sidebar(frame, app, columns[0], theme);
            Self::content(frame, app, columns[1], theme);
        }
        if search_height > 0 {
            Self::search(frame, app, areas[3], theme);
        }
        if message_height > 0 {
            Self::message(frame, app, areas[4], theme);
        }
        Self::footer(frame, app, areas[5], theme);
        if app.show_help {
            Self::help(frame, size, theme);
        } else if app.show_readiness {
            Self::readiness(frame, app, size, theme);
        } else if app.show_action_menu {
            Self::actions(frame, app, size, theme);
        }
    }

    fn brand(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let title = Line::from(vec![
            Span::styled(" PRCTRL ", theme.text(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(" /  review desk", theme.text(TEXT)),
        ]);
        let subtitle = format!(
            " {}  /  {}  /  {} repositories",
            app.config.github_org,
            app.config.github_username,
            app.config.github_repos.len()
        );
        frame.render_widget(
            Paragraph::new(vec![title, Line::styled(subtitle, theme.text(MUTED))]).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(theme.text(BORDER)),
            ),
            area,
        );
    }

    fn tabs(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let spans: Vec<Span> = app
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let name = if area.width >= 55 {
                    format!(" {} {} ", i + 1, tab.short_name())
                } else {
                    format!(" {} ", i + 1)
                };
                Span::styled(
                    name,
                    if *tab == app.active_tab {
                        theme.selected()
                    } else {
                        theme.text(MUTED)
                    },
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn sidebar(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(if area.height >= 16 { 6 } else { 0 }),
            ])
            .split(area);
        let items: Vec<ListItem> = app
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| ListItem::new(format!("{}  {}", i + 1, tab.short_name())))
            .collect();
        let selected = app.tabs.iter().position(|tab| *tab == app.active_tab);
        frame.render_stateful_widget(
            List::new(items)
                .block(theme.block(" Workspace "))
                .highlight_symbol("> ")
                .highlight_style(theme.selected()),
            parts[0],
            &mut ListState::default().with_selected(selected),
        );
        if parts[1].height > 0 {
            frame.render_widget(
                Paragraph::new(
                    "Tab   switch view\n/     find a PR\nr     sync GitHub\n?     all shortcuts",
                )
                .style(theme.text(MUTED))
                .block(theme.block(" Quick keys ")),
                parts[1],
            );
        }
    }

    fn content(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);
        let status = if app.loading {
            format!(
                "{} Syncing GitHub  /  keyboard stays active",
                ["|", "/", "-", "\\"][app.spinner_frame % 4]
            )
        } else if app.error.is_some() {
            "Sync failed  /  r retry  /  previous data retained".into()
        } else if let Some(last) = app.last_refresh {
            let age = (chrono::Utc::now() - last).num_seconds().max(0);
            if app.refresh_interval == 0 {
                format!("Updated {age}s ago  /  automatic sync off")
            } else {
                format!(
                    "Updated {age}s ago  /  next sync in {}s",
                    app.next_refresh_duration().as_secs()
                )
            }
        } else {
            "Waiting for first sync".into()
        };
        let count = if app.filter.is_empty() {
            format!("{} PRs", app.reviews.len())
        } else {
            format!("{} / {} PRs", app.filtered_indices.len(), app.reviews.len())
        };
        frame.render_widget(
            Paragraph::new(status)
                .style(theme.text(MUTED))
                .block(theme.block(format!(" {}  /  {} ", app.active_tab.short_name(), count))),
            parts[0],
        );
        if app.active_tab == Tab::Statistics {
            Self::statistics(frame, app, parts[1], theme);
            return;
        }
        if app.filtered_indices.is_empty() {
            Self::empty(frame, app, parts[1], theme);
            return;
        }
        if area.width >= 125 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(38)])
                .split(parts[1]);
            Self::list(frame, app, columns[0], theme);
            Self::details(frame, app, columns[1], theme);
        } else {
            let detail_height = if parts[1].height >= 18 {
                11
            } else if parts[1].height >= 14 {
                9
            } else {
                0
            };
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(detail_height)])
                .split(parts[1]);
            Self::list(frame, app, rows[0], theme);
            if detail_height > 0 {
                Self::details(frame, app, rows[1], theme);
            }
        }
    }

    fn list(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
        let block = theme.block(" Pull requests ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let narrow = inner.width < 55;
        let row_height = if narrow { 2 } else { 1 };
        let visible = app.visible_range(inner.height.saturating_sub(1) as usize / row_height);
        let wide = inner.width >= 85;
        let now = chrono::Utc::now();
        let rows = app.filtered_indices[visible.clone()].iter().map(|&idx| {
            let pr = &app.reviews[idx];
            let age = age(pr.created_at, now);
            let state = if pr.draft {
                "DRAFT"
            } else if (now - pr.created_at).num_days() > 7 {
                "OVERDUE"
            } else {
                "OPEN"
            };
            let cells = if narrow {
                vec![Cell::from(Text::from(vec![
                    Line::from(format!("#{}  {}", pr.pr_number, pr.pr_title)),
                    Line::from(format!("{} / {}  {age}  {state}", pr.repo, pr.pr_author)),
                ]))]
            } else if wide {
                vec![
                    Cell::from(format!("#{}", pr.pr_number)),
                    Cell::from(pr.pr_title.as_str()),
                    Cell::from(pr.repo.as_str()),
                    Cell::from(pr.pr_author.as_str()),
                    Cell::from(age),
                    Cell::from(state),
                ]
            } else {
                vec![
                    Cell::from(format!("#{}", pr.pr_number)),
                    Cell::from(pr.pr_title.as_str()),
                    Cell::from(age),
                    Cell::from(state),
                ]
            };
            Row::new(cells).height(row_height as u16)
        });
        let (headers, widths) = if narrow {
            (vec!["PR / Repository / Author"], vec![Constraint::Min(0)])
        } else if wide {
            (
                vec!["PR", "Title", "Repository", "Author", "Age", "State"],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(12),
                    Constraint::Length(15),
                    Constraint::Length(13),
                    Constraint::Length(5),
                    Constraint::Length(7),
                ],
            )
        } else {
            (
                vec!["PR", "Title", "Age", "State"],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(8),
                    Constraint::Length(5),
                    Constraint::Length(7),
                ],
            )
        };
        let table = Table::new(rows, widths)
            .header(Row::new(headers).style(theme.text(ACCENT).add_modifier(Modifier::BOLD)))
            .column_spacing(1)
            .row_highlight_style(theme.selected())
            .highlight_symbol("> ");
        let selected = if visible.is_empty() {
            None
        } else {
            Some(app.filtered_position - visible.start)
        };
        frame.render_stateful_widget(
            table,
            inner,
            &mut TableState::default().with_selected(selected),
        );
    }

    fn details(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let Some(pr) = app.selected_pr_item() else {
            return;
        };
        let mut lines = vec![
            Line::styled(
                pr.pr_title.as_str(),
                theme.text(TEXT).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("{} / #{}  by {}", pr.repo, pr.pr_number, pr.pr_author),
                theme.text(MUTED),
            ),
            Line::from(format!(
                "+{} / -{} lines  /  {}",
                pr.additions,
                pr.deletions,
                if pr.draft { "Draft" } else { "Open" }
            )),
            Line::styled(format!("Branch: {}", pr.branch), theme.text(MUTED)),
            Line::styled(pr.pr_url.as_str(), theme.text(ACCENT)),
        ];
        let evidence = app.readiness.observation(&pr.repo, pr.pr_number);
        let mut summary = vec![Line::styled(
            format!(
                "Readiness: {}  /  d details",
                evidence
                    .map(|r| r.state.label())
                    .unwrap_or("UNKNOWN (loading)")
            ),
            theme.text(ACCENT),
        )];
        if let Some(r) = evidence {
            summary.push(Line::from(format!(
                "CI: {} / Review: {}",
                r.ci_status, r.review_decision
            )));
            summary.push(Line::from(format!("Merge: {}", r.merge_state)));
            for blocker in r.blockers.iter().take(2) {
                summary.push(Line::from(format!("- {blocker}")));
            }
            if r.blockers.len() > 2 {
                summary.push(Line::from("More blockers: press d"));
            }
            summary.push(Line::styled(
                format!(
                    "Head: {} / {} UTC",
                    r.head_sha.as_deref().map(|s| &s[..8]).unwrap_or("unknown"),
                    r.observed_at.format("%H:%M:%S")
                ),
                theme.text(MUTED),
            ));
        }
        lines.splice(2..2, summary);
        frame.render_widget(
            Paragraph::new(lines).block(theme.block(" Selected PR  /  o open ")),
            area,
        );
    }

    fn readiness(frame: &mut Frame, app: &mut App, area: Rect, theme: Theme) {
        let popup = centered(area, 100, area.height.saturating_sub(2));
        let block = theme.block(if popup.width < 65 {
            " Readiness / j/k / d close "
        } else {
            " Readiness / j/k scroll / r sync / d or Esc close "
        });
        let inner = block.inner(popup);
        let mut text = Vec::new();
        if let Some(pr) = app.selected_pr_item() {
            text.push(format!("{} / #{} - {}", pr.repo, pr.pr_number, pr.pr_title));
            if let Some(r) = app.readiness.observation(&pr.repo, pr.pr_number) {
                text.push(format!(
                    "{} / CI: {} / Review: {} / Merge: {}",
                    r.state.label(),
                    r.ci_status,
                    r.review_decision,
                    r.merge_state
                ));
                text.push(format!(
                    "Head: {}",
                    r.head_sha.as_deref().unwrap_or("unknown")
                ));
                text.push(format!("Observed: {}", r.observed_at.to_rfc3339()));
                text.push("".into());
                text.push("Blockers / uncertainty:".into());
                if r.blockers.is_empty() {
                    text.push("None reported".into());
                }
                text.extend(r.blockers.iter().map(|s| format!("- {s}")));
                text.push("".into());
                text.push("Checks on head commit:".into());
                text.extend(r.checks.iter().map(|c| format!("{}: {}", c.name, c.status)));
            } else {
                text.push("UNKNOWN: waiting for fresh GitHub evidence".into());
            }
        }
        text.push("".into());
        text.push(
            "Read-only snapshot. GitHub enforces rules and permissions at merge time.".into(),
        );
        let lines = wrap_lines(text, inner.width as usize);
        let max_scroll = lines
            .len()
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
        app.readiness_scroll = app.readiness_scroll.min(max_scroll);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .style(theme.text(TEXT))
                .scroll((app.readiness_scroll, 0)),
            inner,
        );
    }

    fn empty(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let message = if app.loading {
            "Loading your pull requests...\nYou can switch views or press q to quit."
        } else if app.error.is_some() {
            "Could not load this view.\nPress r to retry the GitHub request."
        } else if !app.filter.is_empty() {
            "No matching pull requests.\n/ edit the filter  /  Esc clear it"
        } else if app.config.github_repos.is_empty() {
            "No repositories configured.\nRun prctrl config init to choose repositories."
        } else if app.active_tab == Tab::Crew && app.config.crew_members.is_empty() {
            "No crew configured; showing pending reviews.\nSet crew_members with prctrl config."
        } else {
            "You're all caught up in this view.\nTry another view or press r to sync."
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(theme.text(MUTED))
                .wrap(Wrap { trim: false })
                .block(theme.block(" Pull requests ")),
            area,
        );
    }

    fn statistics(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let s = &app.summary;
        let lines = vec![
            Line::styled(
                "Pending review snapshot",
                theme.text(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::from(""),
            Line::from(format!("Pull requests    {}", app.reviews.len())),
            Line::from(format!("Over 7 days      {}", s.older)),
            Line::from(format!("Drafts           {}", s.drafts)),
            Line::from(format!("Authors          {}", s.authors)),
            Line::from(format!("Repositories     {}", s.repos)),
            Line::from(format!("Lines added      +{}", s.additions)),
            Line::from(format!("Lines removed    -{}", s.deletions)),
            Line::from(""),
            Line::styled(
                "Same dataset as Reviews. Filters apply to the PR list only.",
                theme.text(MUTED),
            ),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme.block(" Overview ")),
            area,
        );
    }

    fn search(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let title = if app.filter_editing {
            " Find PR  /  Enter apply  /  Esc cancel "
        } else {
            " Filter active  /  / edit  /  Esc clear "
        };
        // Show the tail during editing so long queries never hide newly typed characters.
        let available = area.width.saturating_sub(5) as usize;
        let mut used = 0;
        let tail: String = app
            .filter
            .chars()
            .rev()
            .take_while(|ch| {
                used += Span::raw(ch.to_string()).width();
                used <= available
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        frame.render_widget(
            Paragraph::new(format!(
                "/ {tail}{}",
                if app.filter_editing { "_" } else { "" }
            ))
            .style(theme.text(ACCENT))
            .block(theme.block(title)),
            area,
        );
    }

    fn message(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let (title, message) = if let Some(error) = &app.error {
            (" Sync error  /  r retry  /  Esc dismiss ", error.as_str())
        } else {
            (
                " Notice  /  Esc dismiss ",
                app.info.as_deref().unwrap_or(""),
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(theme.text(WARNING))
                .block(theme.block(title)),
            area,
        );
    }

    fn footer(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let count = app.filtered_indices.len();
        let position = if count == 0 {
            "0 / 0".into()
        } else {
            format!("{} / {}", app.filtered_position + 1, count)
        };
        let hints = if app.active_tab == Tab::Statistics {
            "Tab view  /  r sync  /  ? help  /  q quit"
        } else if app.filter_editing {
            "Type to filter  /  Enter apply  /  Esc cancel  /  Ctrl+C quit"
        } else if area.width < 65 {
            "j/k move  / search  d details  ? help  q quit"
        } else {
            "j/k move  Tab view  / search  Enter actions  d details  o open  r sync  ? help  q quit"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(hints, theme.text(MUTED)),
                Line::styled(
                    format!("{position}  /  {}", app.active_tab),
                    theme.text(ACCENT),
                ),
            ]),
            area,
        );
    }

    fn help(frame: &mut Frame, area: Rect, theme: Theme) {
        let popup = centered(area, 64, 20);
        let text = "Move         Up/Down or j/k\nPage         PageUp/PageDown (10 PRs)\nFirst/last   Home/End (filtered results)\nViews        Tab/Shift+Tab or 1-5\nSearch       /, type, Enter to apply\nCancel edit  Esc restores previous filter\nClear filter Esc or Ctrl+F\nOpen PR      o, or Enter then choose\nReadiness    d (j/k scroll, r refresh)\nSync now     r or Ctrl+R\nClose panel  Esc\nQuit         q or Ctrl+C\n\nDRAFT / OPEN / OVERDUE are text labels.\nSelection is marked with >.\nSet NO_COLOR=1 for a monochrome view.\nReview mutations remain available in the CLI.";
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(text)
                .style(theme.text(TEXT))
                .wrap(Wrap { trim: false })
                .block(theme.block(" Keyboard guide  /  Esc close ")),
            popup,
        );
    }

    fn actions(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
        let popup = centered(area, 56, 8);
        let block = theme.block(" PR actions  /  Esc close ");
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);
        if let Some(pr) = app.selected_pr_item() {
            frame.render_widget(
                Paragraph::new(format!("{} / #{}\n{}", pr.repo, pr.pr_number, pr.pr_title))
                    .style(theme.text(MUTED)),
                sections[0],
            );
        }
        let items: Vec<ListItem> = PrAction::all()
            .iter()
            .map(|action| ListItem::new(action.display()))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(theme.selected()),
            sections[1],
            &mut ListState::default().with_selected(Some(app.selected_action)),
        );
        frame.render_widget(
            Paragraph::new("j/k choose  /  Enter select").style(theme.text(MUTED)),
            sections[2],
        );
    }
}

fn wrap_lines(text: Vec<String>, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for text in text {
        let mut line = String::new();
        let mut used = 0;
        for ch in text.chars() {
            let size = if ch.is_ascii() {
                1
            } else {
                Span::raw(ch.to_string()).width()
            };
            if used + size > width.max(1) && !line.is_empty() {
                lines.push(Line::from(std::mem::take(&mut line)));
                used = 0;
            }
            line.push(ch);
            used += size;
        }
        lines.push(Line::from(line));
    }
    lines
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn age(timestamp: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let duration = now - timestamp;
    if duration.num_days() > 0 {
        format!("{}d", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{}h", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{}m", duration.num_minutes())
    } else {
        format!("{}s", duration.num_seconds().max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{config, review};
    use ratatui::{backend::TestBackend, Terminal};

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| Ui::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn layouts_and_overlays_render_at_small_medium_and_wide_sizes() {
        let mut app = App::new(config(), 30).await.unwrap();
        let mut prs: Vec<_> = (1..=60).map(review).collect();
        prs[0].pr_title = "Unicode: café / 東京 / 🦀 / a very long pull request title".into();
        prs[0].draft = true;
        app.finish_load(Tab::PendingReviews, Ok(prs)).await;
        for (width, height) in [
            (0, 0),
            (1, 1),
            (20, 5),
            (30, 10),
            (40, 16),
            (80, 24),
            (120, 32),
            (170, 36),
        ] {
            for tab in Tab::all() {
                app.active_tab = tab;
                render(&mut app, width, height);
                app.filter_editing = true;
                app.error = Some("Network unavailable".into());
                render(&mut app, width, height);
                app.show_help = true;
                render(&mut app, width, height);
                app.show_help = false;
                app.show_action_menu = true;
                render(&mut app, width, height);
                app.show_action_menu = false;
                app.filter_editing = false;
                app.error = None;
            }
        }
        app.active_tab = Tab::PendingReviews;
        let text = render(&mut app, 120, 32);
        assert!(text.contains("DRAFT"));
        assert!(text.contains("Selected PR"));
        app.use_color = false;
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| Ui::draw(frame, &mut app)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));
    }

    #[tokio::test]
    async fn selection_and_empty_results_are_visible_without_color() {
        let mut app = App::new(config(), 0).await.unwrap();
        app.finish_load(Tab::PendingReviews, Ok((1..=100).map(review).collect()))
            .await;
        app.use_color = false;
        app.select_position(99);
        let text = render(&mut app, 80, 24);
        assert!(text.contains("> #100"));
        assert!(text.contains("automatic sync off"));
        app.filter = "no match".into();
        app.update_filtered_indices();
        assert!(render(&mut app, 80, 24).contains("No matching pull requests"));
        assert!(!app.show_action_menu);
    }
    #[tokio::test]
    async fn readiness_panel_exposes_evidence_and_scrolls_without_color() {
        let mut app = App::new(config(), 0).await.unwrap();
        app.finish_load(Tab::PendingReviews, Ok(vec![review(7)]))
            .await;
        let mut evidence = crate::readiness::Readiness::unknown("Required reviews are outstanding");
        evidence.state = crate::readiness::ReadinessState::Blocked;
        evidence.head_sha = Some("a".repeat(40));
        evidence.ci_status = "failure".into();
        evidence.review_decision = "CHANGES_REQUESTED".into();
        evidence.merge_state = "BLOCKED".into();
        evidence.checks = (0..100)
            .map(|n| crate::readiness::CheckResult {
                name: format!("Test {n}"),
                status: "FAILURE".into(),
            })
            .collect();
        app.readiness.fixture(("demo".into(), 7), evidence);
        app.show_readiness = true;
        for (width, height) in [(30, 10), (40, 16), (80, 24), (120, 32), (170, 36)] {
            app.readiness_scroll = 0;
            let text = render(&mut app, width, height);
            assert!(text.contains("BLOCKED"), "{width}x{height}");
            if width >= 80 {
                assert!(text.contains(&"a".repeat(40)));
                assert!(text.contains("Required reviews are outstanding"));
            }
            if let Ok(dir) = std::env::var("PRCTRL_TEST_CAPTURE_DIR") {
                let path = std::path::Path::new(&dir);
                std::fs::create_dir_all(path).unwrap();
                let capture = text
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(width as usize)
                    .map(|row| row.iter().collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(
                    path.join(format!("readiness-{width}x{height}.txt")),
                    capture,
                )
                .unwrap();
            }
            app.readiness_scroll = u16::MAX;
            let text = render(&mut app, width, height);
            assert!(text.contains("merge time."));
        }
        app.use_color = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| Ui::draw(frame, &mut app)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));
    }
}
