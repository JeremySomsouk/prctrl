use crate::github::PendingReview;
use crate::tui::app::{App, PrAction, Tab};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListDirection, ListItem, Paragraph, Row, Table,
};
use ratatui::Frame;

/// Main UI renderer
pub struct Ui;

impl Ui {
    /// Draw the entire UI
    pub fn draw(frame: &mut Frame, app: &mut App) {
        let size = frame.area();

        // Create main layout with sidebar and content
        let areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(25), // Sidebar width (fixed) - widened to show full tab names
                Constraint::Min(60),    // Main content
            ])
            .split(size);

        // Draw sidebar with command tabs
        Self::draw_sidebar(frame, app, areas[0]);

        // Draw main content area
        Self::draw_main_content(frame, app, areas[1]);

        // Draw action menu if shown
        if app.show_action_menu {
            Self::draw_action_menu(frame, app, size);
        }
        // Draw help overlay if shown
        else if app.show_help {
            Self::draw_help_overlay(frame, app, size);
        }

        // Draw loading indicator if loading
        if app.loading {
            if app.reviews.is_empty() {
                // Initial load: show full loading screen
                Self::draw_loading_indicator(frame, app, size);
            } else {
                // Refreshing: show small spinner in corner
                Self::draw_refresh_spinner(frame, app, size);
            }
        }

        // Draw info message if present (shows before error)
        if let Some(info) = &app.info {
            Self::draw_info_message(frame, info, size);
        }

        // Draw error message if present
        if let Some(error) = &app.error {
            Self::draw_error_message(frame, error, size);
        }
    }

    /// Draw the left sidebar with command tabs
    fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
        let tabs = &app.tabs;
        let active_tab = &app.active_tab;

        // Create a list of tab items
        let tab_items: Vec<ListItem> = tabs
            .iter()
            .map(|tab| {
                let icon = tab.icon();
                let name = format!(" {}", tab);
                let is_active = *tab == *active_tab;

                let style = if is_active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                        .bg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Gray)
                };

                let content = Span::styled(format!("{} {}", icon, name), style);
                ListItem::new(content)
            })
            .collect();

        // Create the list widget
        let list = List::new(tab_items)
            .direction(ListDirection::TopToBottom)
            .block(
                Block::default()
                    .title(Span::styled(
                        " Commands ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::LightCyan),
            )
            .highlight_symbol("> ");

        // Find the index of the active tab for highlighting
        let active_index = tabs.iter().position(|t| *t == *active_tab).unwrap_or(0);

        // Render the list
        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(active_index));

        frame.render_stateful_widget(list, area, &mut state);

        // Draw footer with keyboard hints
        Self::draw_sidebar_footer(frame, area);
    }

    /// Draw footer in sidebar with keyboard hints
    fn draw_sidebar_footer(frame: &mut Frame, area: Rect) {
        let footer_text = vec![
            Span::styled("↑↓ ", Color::Gray),
            Span::styled("Navigate ", Color::White),
            Span::styled("Tab ", Color::Gray),
            Span::styled("Switch ", Color::White),
            Span::styled("q ", Color::Gray),
            Span::styled("Quit", Color::White),
        ];

        let footer = Paragraph::new(Line::from(footer_text))
            .block(Block::default())
            .alignment(Alignment::Center);

        let footer_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };

        frame.render_widget(footer, footer_area);
    }

    /// Draw the main content area
    fn draw_main_content(frame: &mut Frame, app: &App, area: Rect) {
        // Split main area into header, content, and footer
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Header
                Constraint::Min(10),   // Content
                Constraint::Length(2), // Footer
            ])
            .split(area);

        // Draw header with current tab title and refresh info
        Self::draw_header(frame, app, areas[0]);

        // Draw PR list or stats based on active tab
        match app.active_tab {
            Tab::Statistics => Self::draw_statistics(frame, app, areas[1]),
            _ => Self::draw_pr_list(frame, app, areas[1]),
        }

        // Draw footer with status
        Self::draw_main_footer(frame, app, areas[2]);
    }

    /// Draw header with current tab title and refresh info
    fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
        let tab_name = format!("{}", app.active_tab);
        let refresh_info = if let Some(last) = app.last_refresh {
            let elapsed = last.signed_duration_since(chrono::Utc::now());
            let seconds = elapsed.num_seconds().unsigned_abs();
            format!(" | Last refresh: {}s ago", seconds)
        } else {
            String::new()
        };

        let title = Span::styled(
            format!("{} {}", tab_name, refresh_info),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let header = Paragraph::new(title)
            .block(
                Block::default()
                    .title(Span::styled(
                        format!(" {} ", app.active_tab.icon()),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .alignment(Alignment::Left);

        frame.render_widget(header, area);
    }

    /// Draw the PR list
    fn draw_pr_list(frame: &mut Frame, app: &App, area: Rect) {
        let filtered_reviews = app.filtered_reviews();

        // Create table rows
        let rows: Vec<Row> = filtered_reviews
            .iter()
            .map(|pr| {
                let age = Self::format_duration(pr.created_at);
                let age_days = (chrono::Utc::now() - pr.created_at).num_days();
                let size = format!("+{}/-{}", pr.additions, pr.deletions);
                let draft = if pr.draft { " [DRAFT]" } else { "" };

                // Style based on age and draft status
                let (number_style, title_style, age_style) = if pr.draft {
                    (
                        Style::default().fg(Color::Gray),
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::ITALIC),
                        Style::default().fg(Color::Gray),
                    )
                } else if age_days > 7 {
                    // Old PRs (> 7 days) - red/orange
                    (
                        Style::default().fg(Color::Red),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Red),
                    )
                } else if age_days > 3 {
                    // Aging PRs (3-7 days) - yellow/orange
                    (
                        Style::default().fg(Color::Yellow),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Yellow),
                    )
                } else {
                    // Recent PRs (< 3 days) - green
                    (
                        Style::default().fg(Color::Green),
                        Style::default().fg(Color::White),
                        Style::default().fg(Color::Green),
                    )
                };

                Row::new(vec![
                    Cell::from(Span::styled(pr.pr_number.to_string(), number_style)),
                    Cell::from(Span::styled(
                        format!("{}{}", pr.pr_title, draft),
                        title_style,
                    )),
                    Cell::from(Span::styled(
                        pr.pr_author.as_str(),
                        Style::default().fg(Color::Cyan),
                    )),
                    Cell::from(Span::styled(
                        pr.repo.as_str(),
                        Style::default().fg(Color::Magenta),
                    )),
                    Cell::from(Span::styled(age, age_style)),
                    Cell::from(Span::styled(size, Style::default().fg(Color::Blue))),
                ])
            })
            .collect();

        // Create table - in ratatui 0.28, Table::new takes rows and widths
        let widths = &[
            Constraint::Length(5),  // Number
            Constraint::Min(40),    // Title (more space for titles)
            Constraint::Length(15), // Author
            Constraint::Length(20), // Repo
            Constraint::Length(8),  // Age
            Constraint::Length(10), // Changes
        ];

        let table = Table::new(rows, widths)
            .header(
                Row::new(vec!["#", "Title", "Author", "Repo", "Age", "Changes"])
                    .style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                    .bottom_margin(1),
            )
            .block(
                Block::default()
                    .borders(Borders::NONE)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
            .row_highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
            .column_spacing(1);

        // Find the selected index in the filtered list
        let selected_index = app.filtered_reviews().iter().position(|&pr| {
            let pr_ptr = pr as *const PendingReview;
            let selected_ptr = &app.reviews[app.selected_pr] as *const PendingReview;
            std::ptr::eq(pr_ptr, selected_ptr)
        });

        // Render table
        let mut state = ratatui::widgets::TableState::default();
        if let Some(index) = selected_index {
            state.select(Some(index));
        }

        frame.render_stateful_widget(table, area, &mut state);

        // Draw filter input if in filter mode
        if !app.filter.is_empty() {
            Self::draw_filter_input(frame, app, area);
        }
    }

    /// Draw filter input overlay
    fn draw_filter_input(frame: &mut Frame, app: &App, area: Rect) {
        let input = format!("/{}", app.filter);

        let filter_widget = Paragraph::new(input)
            .block(
                Block::default()
                    .title("Filter (press Esc to clear)")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .style(Style::default().fg(Color::Yellow));

        // Position at top of content area
        let filter_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width.min(50),
            height: 3,
        };

        frame.render_widget(Clear, filter_area);
        frame.render_widget(filter_widget, filter_area);
    }

    /// Draw statistics view
    fn draw_statistics(frame: &mut Frame, app: &App, area: Rect) {
        let reviews = &app.reviews;

        let total_prs = reviews.len();
        let draft_prs = reviews.iter().filter(|pr| pr.draft).count();
        let total_additions: u64 = reviews.iter().map(|pr| pr.additions).sum();
        let total_deletions: u64 = reviews.iter().map(|pr| pr.deletions).sum();

        // Get unique authors and repos
        let authors: std::collections::HashSet<_> =
            reviews.iter().map(|pr| &pr.pr_author).collect();
        let repos: std::collections::HashSet<_> = reviews.iter().map(|pr| &pr.repo).collect();

        let stats_text = vec![
            Line::from(vec![
                Span::styled("Total PRs: ", Color::Cyan),
                Span::styled(format!("{}", total_prs), Color::White),
            ]),
            Line::from(vec![
                Span::styled("Draft PRs: ", Color::Cyan),
                Span::styled(format!("{}", draft_prs), Color::Yellow),
            ]),
            Line::from(vec![
                Span::styled("Total +Lines: ", Color::Cyan),
                Span::styled(format!("{}", total_additions), Color::Green),
            ]),
            Line::from(vec![
                Span::styled("Total -Lines: ", Color::Cyan),
                Span::styled(format!("{}", total_deletions), Color::Red),
            ]),
            Line::from(vec![
                Span::styled("Unique Authors: ", Color::Cyan),
                Span::styled(format!("{}", authors.len()), Color::White),
            ]),
            Line::from(vec![
                Span::styled("Repositories: ", Color::Cyan),
                Span::styled(format!("{}", repos.len()), Color::White),
            ]),
        ];

        let stats = Paragraph::new(stats_text)
            .block(
                Block::default()
                    .title("Statistics")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Gray)),
            )
            .alignment(Alignment::Left);

        frame.render_widget(stats, area);
    }

    /// Draw main footer with status and keyboard hints
    fn draw_main_footer(frame: &mut Frame, app: &App, area: Rect) {
        let filtered_count = app.filtered_reviews().len();
        let total_count = app.reviews.len();

        let status_text = if app.filter.is_empty() {
            format!("{} PRs", filtered_count)
        } else {
            format!("{} of {} PRs (filtered)", filtered_count, total_count)
        };

        let next_refresh = if app.loading {
            String::from("Refreshing...")
        } else {
            let duration = app.next_refresh_duration();
            format!("Refresh in {}s", duration.as_secs())
        };

        let mut footer_parts = vec![
            Span::styled(status_text, Color::White),
            Span::styled(" | ", Color::Gray),
            Span::styled(next_refresh, Color::Cyan),
        ];

        // Add filter indicator if active
        if !app.filter.is_empty() {
            footer_parts.extend(vec![
                Span::styled(" | ", Color::Gray),
                Span::styled("🔍 ", Color::Yellow),
                Span::styled(&app.filter, Color::LightYellow),
            ]);
        }

        // Add keyboard hints
        footer_parts.extend(vec![
            Span::styled(" | ", Color::Gray),
            Span::styled("r ", Color::Yellow),
            Span::styled("Refresh ", Color::White),
            Span::styled("| ", Color::Gray),
            Span::styled("/ ", Color::Yellow),
            Span::styled("Filter ", Color::White),
            Span::styled("| ", Color::Gray),
            Span::styled("Enter ", Color::Yellow),
            Span::styled("Actions ", Color::White),
            Span::styled("| ", Color::Gray),
            Span::styled("? ", Color::Yellow),
            Span::styled("Help ", Color::White),
        ]);

        let footer = Paragraph::new(Line::from(footer_parts))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .alignment(Alignment::Left);

        frame.render_widget(footer, area);
    }

    /// Draw help overlay
    fn draw_help_overlay(frame: &mut Frame, _app: &App, size: Rect) {
        let help_text = Text::from(vec![
            Line::from(vec![Span::styled(
                "PRCtrl TUI - Keyboard Shortcuts",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![]),
            Line::from(vec![Span::styled(
                "Navigation:",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("  j / ↓ ", Color::Green),
                Span::styled(" - Move down", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  k / ↑ ", Color::Green),
                Span::styled(" - Move up", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  Tab ", Color::Green),
                Span::styled(" - Next tab", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  Shift+Tab ", Color::Green),
                Span::styled(" - Previous tab", Color::White),
            ]),
            Line::from(vec![]),
            Line::from(vec![Span::styled(
                "Actions:",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("  r ", Color::Green),
                Span::styled(" - Refresh", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  / ", Color::Green),
                Span::styled(" - Start filtering", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  Esc ", Color::Green),
                Span::styled(" - Clear filter / Close help", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  Enter ", Color::Green),
                Span::styled(" - Select PR", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  ? ", Color::Green),
                Span::styled(" - Toggle help", Color::White),
            ]),
            Line::from(vec![]),
            Line::from(vec![Span::styled(
                "Quit:",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("  q ", Color::Green),
                Span::styled(" - Quit", Color::White),
            ]),
            Line::from(vec![
                Span::styled("  Esc ", Color::Green),
                Span::styled(" - Quit", Color::White),
            ]),
        ]);

        let help_widget = Paragraph::new(help_text).block(
            Block::default()
                .title("Help")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );

        // Center the help dialog
        let help_area = Rect {
            x: size.x + size.width / 4,
            y: size.y + size.height / 4,
            width: size.width / 2,
            height: size.height / 2,
        };

        // Clear the background
        frame.render_widget(Clear, help_area);
        frame.render_widget(help_widget, help_area);
    }

    /// Draw full loading screen (for initial load when no reviews yet)
    fn draw_loading_indicator(frame: &mut Frame, app: &App, size: Rect) {
        use ratatui::symbols::border;

        let spinner = Self::spinner(app.spinner_frame);

        // Create a loading message
        let loading_text = vec![
            Line::from(vec![
                Span::styled(spinner, Color::LightYellow),
                Span::styled(" Loading PRs...", Color::Cyan),
            ]),
            Line::from(vec![
                Span::styled(" ", Color::Gray), // Empty line for spacing
            ]),
            Line::from(Span::styled(
                "Please wait while we fetch your reviews",
                Color::Gray,
            )),
        ];

        let loading = Paragraph::new(loading_text)
            .block(
                Block::default()
                    .title(Span::styled(
                        " Loading ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue))
                    .border_set(border::ROUNDED),
            )
            .alignment(Alignment::Center);

        // Center the loading indicator
        let loading_width = 40.min(size.width.saturating_sub(4));
        let loading_height = 5.min(size.height.saturating_sub(4));

        let loading_area = Rect {
            x: (size.width.saturating_sub(loading_width)) / 2,
            y: (size.height.saturating_sub(loading_height)) / 2,
            width: loading_width,
            height: loading_height,
        };

        // Clear the area behind the loading indicator
        frame.render_widget(Clear, loading_area);
        frame.render_widget(loading, loading_area);
    }

    /// Draw small spinner in corner (for refreshing when reviews exist)
    fn draw_refresh_spinner(frame: &mut Frame, app: &App, size: Rect) {
        let spinner = Self::spinner(app.spinner_frame);
        let spinner_text = vec![
            Span::styled(spinner, Color::LightYellow),
            Span::styled(" Loading...", Color::Cyan),
        ];

        let spinner_widget = Paragraph::new(Line::from(spinner_text))
            .block(Block::default())
            .alignment(Alignment::Right);

        let spinner_area = Rect {
            x: size.width.saturating_sub(25),
            y: size.height.saturating_sub(1),
            width: 25,
            height: 1,
        };

        frame.render_widget(spinner_widget, spinner_area);
    }

    /// Draw info message (non-error notification)
    fn draw_info_message(frame: &mut Frame, info: &str, size: Rect) {
        use ratatui::symbols::border;

        let info_text = vec![
            Span::styled("ℹ️  ", Color::Cyan),
            Span::styled(info, Color::LightCyan),
        ];

        let info_widget = Paragraph::new(Line::from(info_text))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Info ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue))
                    .border_set(border::ROUNDED),
            )
            .alignment(Alignment::Left);

        let info_area = Rect {
            x: size.x + size.width / 4,
            y: size.y + size.height / 3,
            width: size.width / 2,
            height: 5,
        };

        frame.render_widget(Clear, info_area);
        frame.render_widget(info_widget, info_area);
    }

    /// Draw error message
    fn draw_error_message(frame: &mut Frame, error: &str, size: Rect) {
        let error_text = vec![
            Span::styled("⚠️  ", Color::Red),
            Span::styled(error, Color::LightRed),
        ];

        let error_widget = Paragraph::new(Line::from(error_text))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Error ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            )
            .alignment(Alignment::Left);

        let error_area = Rect {
            x: size.x + size.width / 4,
            y: size.y + size.height / 2,
            width: size.width / 2,
            height: 5,
        };

        frame.render_widget(Clear, error_area);
        frame.render_widget(error_widget, error_area);
    }

    /// Simple spinner animation - returns spinner character based on frame
    fn spinner(frame: usize) -> &'static str {
        const SPINNER_FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
        SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
    }

    /// Format duration from a timestamp
    fn format_duration(timestamp: chrono::DateTime<chrono::Utc>) -> String {
        let now = chrono::Utc::now();
        let duration = now.signed_duration_since(timestamp);

        if duration.num_days() > 0 {
            format!("{}d", duration.num_days())
        } else if duration.num_hours() > 0 {
            format!("{}h", duration.num_hours())
        } else if duration.num_minutes() > 0 {
            format!("{}m", duration.num_minutes())
        } else {
            format!("{}s", duration.num_seconds())
        }
    }

    /// Draw the action menu overlay for selected PR
    fn draw_action_menu(frame: &mut Frame, app: &App, area: Rect) {
        use ratatui::symbols::border;

        let actions = PrAction::all();

        // Create a centered popup
        let popup_width = 50;
        let popup_height = actions.len() as u16 + 5; // +5 for padding, border, and help

        let popup_area = Rect {
            x: (area.width.saturating_sub(popup_width)) / 2,
            y: (area.height.saturating_sub(popup_height)) / 2,
            width: popup_width.min(area.width),
            height: popup_height.min(area.height),
        };

        // Create a block with border
        let block = Block::default()
            .title(Span::styled(
                " Actions ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue))
            .border_set(border::ROUNDED);

        // Draw the block (clears the area)
        frame.render_widget(Clear, popup_area);
        frame.render_widget(block, popup_area);

        // Create list of actions with styled icons
        let action_items: Vec<ListItem> = actions
            .iter()
            .map(|action| {
                let icon = action.icon();
                let display = action.display();
                let content = Line::from(vec![
                    Span::styled(icon, Style::default().fg(Color::Cyan)),
                    Span::styled(" ", Style::default()),
                    Span::styled(display, Style::default().fg(Color::White)),
                ]);
                ListItem::new(content)
            })
            .collect();

        // Create list widget with highlighted selection
        let list = List::new(action_items)
            .direction(ListDirection::TopToBottom)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("❯ ");

        // Draw selected PR info at the top
        if let Some(pr) = app.selected_pr_item() {
            let pr_info = Span::styled(
                format!("PR #{} - {}", pr.pr_number, pr.pr_title),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
            let info_area = Rect {
                x: popup_area.x + 2,
                y: popup_area.y + 1,
                width: popup_area.width.saturating_sub(4),
                height: 1,
            };
            frame.render_widget(Paragraph::new(Line::from(pr_info)), info_area);
        }

        // Render list below the PR info
        let inner_area = Rect {
            x: popup_area.x + 2,
            y: popup_area.y + 3, // Start below PR info and title
            width: popup_area.width.saturating_sub(4),
            height: popup_area.height.saturating_sub(5), // Leave room for PR info, title, and help
        };

        frame.render_stateful_widget(
            list,
            inner_area,
            &mut ratatui::widgets::ListState::default().with_selected(Some(app.selected_action)),
        );

        // Draw instructions at the bottom
        let help_text = Span::styled(
            "↑/↓ Navigate | Enter Select | Esc Cancel",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        );
        let help_area = Rect {
            x: popup_area.x + 2,
            y: popup_area.y + popup_area.height - 2,
            width: popup_area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(help_text)), help_area);
    }
}
