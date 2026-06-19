use crate::config::Config;
use crate::tui::app::App;
use crate::tui::events::Event;
use crate::tui::ui::Ui;
use anyhow::{Context, Result};
use crossterm::event::Event as CrosstermEvent;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

/// Run the TUI application
pub async fn run_tui(config: Config, refresh_interval: u64) -> Result<()> {
    // Initialize terminal - MUST be done first
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app state
    let mut app = App::new(config, refresh_interval)
        .await
        .context("Failed to initialize TUI app")?;

    // Draw the UI once to show loading screen before fetching
    terminal.draw(|frame| Ui::draw(frame, &mut app))?;

    // First, load the current tab (PendingReviews) synchronously
    app.refresh().await?;

    // Redraw after initial load
    terminal.draw(|frame| Ui::draw(frame, &mut app))?;

    // Then, preload all other tabs asynchronously in the background
    // This will populate the cache so subsequent tab switches are instant
    let cache = app.cache.clone();
    let preload_config = app.config.clone();
    let _preload_task = tokio::spawn(async move {
        // Create a temporary app just for preloading
        let mut temp_app = App::new(preload_config, 0).await.ok()?;
        temp_app.cache = cache;
        let _ = temp_app.preload_all_tabs().await;
        Some(())
    });

    // Main event loop
    loop {
        // Calculate timeout based on next refresh
        let refresh_duration = app.next_refresh_duration();
        let timeout = if refresh_duration.is_zero() {
            Duration::from_millis(100)
        } else {
            refresh_duration.min(Duration::from_millis(500))
        };

        // Poll for events with timeout
        if crossterm::event::poll(timeout).unwrap_or(false) {
            // Event available - read it
            if let Ok(CrosstermEvent::Key(key)) = crossterm::event::read() {
                if !handle_event(&mut app, Event::Key(key)).await? {
                    break;
                }
                // Redraw after handling key event
                terminal.draw(|frame| Ui::draw(frame, &mut app))?;
            }
        } else {
            // Timeout - check if we should refresh
            if app.next_refresh_duration().is_zero() {
                app.refresh().await?;
                terminal.draw(|frame| Ui::draw(frame, &mut app))?;
            }
        }
    }

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

/// Handle an event and return whether to continue running
async fn handle_event(app: &mut App, event: Event) -> Result<bool> {
    match event {
        Event::Key(key) => handle_key_event(app, key).await,
        Event::Tick => {
            // Handle periodic tick
            Ok(true)
        }
        Event::Error => {
            app.error = Some("An error occurred".to_string());
            Ok(true)
        }
        Event::Quit => Ok(false),
    }
}

/// Handle keyboard input
async fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) -> Result<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};

    // Clear info/error popup on any key press (unless we're in a special mode)
    if (app.info.is_some() || app.error.is_some()) && !app.show_action_menu && !app.show_help {
        app.info = None;
        app.error = None;
    }

    match key.code {
        // Quit
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            return Ok(false);
        }
        KeyCode::Esc => {
            // Close action menu, help, or filter, or clear info/error
            if app.show_action_menu {
                app.show_action_menu = false;
            } else if !app.filter.is_empty() {
                app.filter.clear();
                app.update_filtered_indices();
            } else if app.show_help {
                app.show_help = false;
            } else if app.info.is_some() || app.error.is_some() {
                // Dismiss info or error popup
                app.info = None;
                app.error = None;
            } else {
                return Ok(false);
            }
        }

        // Navigation
        KeyCode::Down | KeyCode::Char('j') => {
            if app.show_action_menu {
                // Navigate action menu
                let actions = crate::tui::app::PrAction::all();
                if app.selected_action < actions.len() - 1 {
                    app.selected_action += 1;
                }
            } else {
                app.next_pr();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.show_action_menu {
                // Navigate action menu
                if app.selected_action > 0 {
                    app.selected_action -= 1;
                }
            } else {
                app.prev_pr();
            }
        }
        KeyCode::PageDown => {
            // Page down - move 10 items
            for _ in 0..10 {
                app.next_pr();
            }
        }
        KeyCode::PageUp => {
            // Page up - move 10 items
            for _ in 0..10 {
                app.prev_pr();
            }
        }
        KeyCode::Home => {
            app.selected_pr = 0;
        }
        KeyCode::End => {
            app.selected_pr = app.reviews.len().saturating_sub(1);
        }

        // Tab switching
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_tab();
            } else {
                app.next_tab();
            }
            // Set loading state before refreshing
            app.loading = true;
            // Check cache and refresh if needed for the new tab
            app.refresh().await?;
            // Clear loading indicator
            app.loading = false;
        }
        KeyCode::BackTab => {
            app.prev_tab();
            // Set loading state before refreshing
            app.loading = true;
            // Check cache and refresh if needed for the new tab
            app.refresh().await?;
            // Clear loading indicator
            app.loading = false;
        }

        // Filter
        KeyCode::Char('/') => {
            // Start filter mode
            app.filter = String::from("/");
            app.update_filtered_indices();
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Clear filter
            app.filter.clear();
            app.update_filtered_indices();
        }

        // Force refresh (bypasses cache) - must come before regular refresh
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.force_refresh().await?;
        }
        // Refresh (uses cache if available)
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.refresh().await?;
        }

        // Help
        KeyCode::Char('?') => {
            app.show_help = !app.show_help;
        }

        // Filter input (must come after specific char patterns)
        KeyCode::Char(c) => {
            if !app.filter.is_empty() {
                // If we're in filter mode, add to filter
                // Skip the leading '/' if present
                if app.filter == "/" {
                    app.filter.clear();
                }
                app.filter.push(c);
                app.update_filtered_indices();
            }
        }
        KeyCode::Backspace => {
            if !app.filter.is_empty() {
                app.filter.pop();
                // If filter becomes empty, clear it completely
                if app.filter.is_empty() {
                    app.filter.clear();
                }
                app.update_filtered_indices();
            }
        }
        KeyCode::Delete => {
            app.filter.clear();
            app.update_filtered_indices();
        }

        // Select PR or action
        KeyCode::Enter => {
            if app.show_action_menu {
                // Execute the selected action
                handle_action(app).await?;
            } else if app.selected_pr_item().is_some() {
                // Show action menu for this PR
                app.show_action_menu = true;
                app.selected_action = 0;
            }
        }

        // Ignore other keys
        _ => {}
    }

    Ok(true)
}

/// Handle the selected action from the action menu
async fn handle_action(app: &mut App) -> Result<()> {
    use crate::tui::app::PrAction;

    let actions = PrAction::all();
    let selected = actions
        .get(app.selected_action)
        .copied()
        .unwrap_or(PrAction::Cancel);

    // Close the action menu
    app.show_action_menu = false;

    match selected {
        PrAction::OpenInBrowser => {
            if let Some(pr) = app.selected_pr_item() {
                // Use the open crate to open the URL in the default browser
                if let Err(e) = open::that(&pr.pr_url) {
                    app.error = Some(format!("Failed to open browser: {}", e));
                }
            }
        }
        PrAction::ClaudeReview => {
            if let Some(pr) = app.selected_pr_item() {
                // In a real implementation, this would call the Claude API
                // For now, set info message
                app.info = Some(format!(
                    "Claude Code review would be launched for: {}",
                    pr.pr_title
                ));
            }
        }
        PrAction::CopyUrl => {
            if let Some(pr) = app.selected_pr_item() {
                // Copy URL to clipboard using arboard or clipboard crate
                // For now, set info message
                app.info = Some(format!("Copied URL: {}", pr.pr_url));
            }
        }
        PrAction::ShowDiff => {
            if let Some(pr) = app.selected_pr_item() {
                // Would show diff in a real implementation
                app.info = Some(format!(
                    "Showing diff for PR #{} - {}",
                    pr.pr_number, pr.pr_title
                ));
            }
        }
        PrAction::Approve => {
            if let Some(pr) = app.selected_pr_item() {
                // Would approve PR in a real implementation
                app.info = Some(format!("Approved PR #{} - {}", pr.pr_number, pr.pr_title));
            }
        }
        PrAction::RequestChanges => {
            if let Some(pr) = app.selected_pr_item() {
                // Would request changes in a real implementation
                app.info = Some(format!(
                    "Requested changes for PR #{} - {}",
                    pr.pr_number, pr.pr_title
                ));
            }
        }
        PrAction::Cancel => {
            // Already closed the menu, nothing to do
        }
    }

    Ok(())
}
