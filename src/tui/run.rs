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

/// Restore the terminal even if drawing, input or a load fails.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Run keyboard input, load completions and animation on independent event sources.
pub async fn run_tui(config: Config, refresh_interval: u64) -> Result<()> {
    use crossterm::event::{EventStream, KeyEventKind};
    use futures::StreamExt;

    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new(config, refresh_interval)
        .await
        .context("Failed to initialize TUI app")?;
    app.refresh().await?;
    app.preload_all_tabs().await?;

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_second = std::time::Instant::now();
    loop {
        let selected = if app.loading || app.active_tab == crate::tui::app::Tab::Statistics {
            None
        } else {
            app.selected_pr_item()
                .map(|pr| (pr.repo.clone(), pr.pr_number))
        };
        app.readiness.select(selected);
        if dirty {
            terminal.draw(|frame| Ui::draw(frame, &mut app))?;
            dirty = false;
        }
        tokio::select! {
            event = input.next() => {
                match event {
                    Some(Ok(CrosstermEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                        if !handle_event(&mut app, Event::Key(key)).await? { break; }
                        dirty = true;
                    }
                    Some(Ok(CrosstermEvent::Resize(..))) => dirty = true,
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                    _ => {}
                }
            }
            result = app.loads.join_next(), if !app.loads.is_empty() => {
                match result {
                    Some(Ok((tab, data))) => app.finish_load(tab, data).await,
                    Some(Err(error)) => return Err(error.into()),
                    None => {}
                }
                dirty = true;
            }
            result = app.readiness.loads.join_next(), if !app.readiness.loads.is_empty() => {
                if let Some(Ok((generation, key, data))) = result { app.readiness.finish(generation, key, data); }
                dirty = true;
            }
            _ = tick.tick() => {
                if !app.loading && app.next_refresh_duration().is_zero() {
                    app.force_refresh().await?;
                    dirty = true;
                }
                if app.loading {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    dirty = true;
                }
                if last_second.elapsed() >= Duration::from_secs(1) {
                    last_second = std::time::Instant::now();
                    dirty = true;
                }
            }
        }
    }
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

/// Route input to the focused control before interpreting global shortcuts.
async fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) -> Result<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return Ok(false);
    }
    if ctrl && key.code == KeyCode::Char('f') {
        app.filter.clear();
        app.filter_editing = false;
        app.update_filtered_indices();
        return Ok(true);
    }
    if app.filter_editing {
        match key.code {
            KeyCode::Enter => app.filter_editing = false,
            KeyCode::Esc => {
                app.filter = app.filter_before_edit.clone();
                app.filter_editing = false;
            }
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Delete => app.filter.clear(),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                app.filter.push(c)
            }
            _ => {}
        }
        app.update_filtered_indices();
        return Ok(true);
    }
    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
        return Ok(false);
    }
    if app.show_help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            app.show_help = false;
        }
        return Ok(true);
    }
    if app.show_readiness {
        match key.code {
            KeyCode::Esc | KeyCode::Char('d') => app.show_readiness = false,
            KeyCode::Down | KeyCode::Char('j') => {
                app.readiness_scroll = app.readiness_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.readiness_scroll = app.readiness_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => app.readiness_scroll = app.readiness_scroll.saturating_add(10),
            KeyCode::PageUp => app.readiness_scroll = app.readiness_scroll.saturating_sub(10),
            KeyCode::Home => app.readiness_scroll = 0,
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.readiness_scroll = 0;
                app.force_refresh().await?;
            }
            _ => {}
        }
        return Ok(true);
    }
    if app.show_action_menu {
        match key.code {
            KeyCode::Esc => app.show_action_menu = false,
            KeyCode::Down | KeyCode::Char('j') => {
                app.selected_action =
                    (app.selected_action + 1).min(crate::tui::app::PrAction::all().len() - 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected_action = app.selected_action.saturating_sub(1)
            }
            KeyCode::Enter => handle_action(app),
            _ => {}
        }
        return Ok(true);
    }
    match key.code {
        KeyCode::Esc => {
            if !app.filter.is_empty() {
                app.filter.clear();
                app.update_filtered_indices();
            } else {
                app.info = None;
                app.error = None;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => app.next_pr(),
        KeyCode::Up | KeyCode::Char('k') => app.prev_pr(),
        KeyCode::PageDown => app.select_position(app.filtered_position.saturating_add(10)),
        KeyCode::PageUp => app.select_position(app.filtered_position.saturating_sub(10)),
        KeyCode::Home => app.select_position(0),
        KeyCode::End => app.select_position(app.filtered_indices.len().saturating_sub(1)),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_tab();
            } else {
                app.next_tab();
            }
            app.refresh().await?;
        }
        KeyCode::BackTab => {
            app.prev_tab();
            app.refresh().await?;
        }
        KeyCode::Char(c @ '1'..='5') => {
            app.set_active_tab(c as usize - '1' as usize);
            app.refresh().await?;
        }
        KeyCode::Char('/') => {
            app.filter_before_edit = app.filter.clone();
            app.filter_editing = true;
        }
        KeyCode::Char('r') | KeyCode::Char('R') => app.force_refresh().await?,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('d')
            if app.selected_pr_item().is_some()
                && app.active_tab != crate::tui::app::Tab::Statistics =>
        {
            app.show_readiness = true;
            app.readiness_scroll = 0;
        }
        KeyCode::Char('o') if app.active_tab != crate::tui::app::Tab::Statistics => {
            open_selected(app)
        }
        KeyCode::Enter
            if app.selected_pr_item().is_some()
                && app.active_tab != crate::tui::app::Tab::Statistics =>
        {
            app.show_action_menu = true;
            app.selected_action = 0;
        }
        _ => {}
    }
    Ok(true)
}

fn open_selected(app: &mut App) {
    if let Some(pr) = app.selected_pr_item() {
        match open::that_detached(&pr.pr_url) {
            Ok(()) => {
                app.error = None;
                app.info = Some("Browser launch requested.".into());
            }
            Err(error) => app.error = Some(format!("Cannot launch browser: {error}")),
        }
    }
}

fn handle_action(app: &mut App) {
    use crate::tui::app::PrAction;
    let selected = PrAction::all()
        .get(app.selected_action)
        .copied()
        .unwrap_or(PrAction::Cancel);
    app.show_action_menu = false;
    if selected == PrAction::OpenInBrowser {
        open_selected(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{config, review};
    use crate::tui::app::Tab;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    async fn key(app: &mut App, code: KeyCode) -> bool {
        handle_key_event(app, KeyEvent::new(code, KeyModifiers::NONE))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn filter_accepts_shortcut_letters_without_running_commands() {
        let mut app = App::new(config(), 30).await.unwrap();
        key(&mut app, KeyCode::Char('/')).await;
        for c in "qrjk?123/".chars() {
            assert!(key(&mut app, KeyCode::Char(c)).await);
        }
        assert_eq!(app.filter, "qrjk?123/");
        assert_eq!(app.active_tab, Tab::PendingReviews);
        assert!(app.loads.is_empty());
        key(&mut app, KeyCode::Enter).await;
        assert!(!app.filter_editing);
        key(&mut app, KeyCode::Char('/')).await;
        key(&mut app, KeyCode::Char('x')).await;
        key(&mut app, KeyCode::Esc).await;
        assert_eq!(app.filter, "qrjk?123/");
        key(&mut app, KeyCode::Esc).await;
        assert!(app.filter.is_empty());
        assert!(key(&mut app, KeyCode::Esc).await);
        assert!(!key(&mut app, KeyCode::Char('q')).await);
    }

    #[tokio::test]
    async fn empty_filter_stays_focused_after_backspace() {
        let mut app = App::new(config(), 30).await.unwrap();
        key(&mut app, KeyCode::Char('/')).await;
        key(&mut app, KeyCode::Char('x')).await;
        key(&mut app, KeyCode::Backspace).await;
        assert!(key(&mut app, KeyCode::Char('q')).await);
        assert_eq!(app.filter, "q");
    }

    #[tokio::test]
    async fn help_and_menus_capture_input_and_empty_results_disable_actions() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.finish_load(Tab::PendingReviews, Ok(vec![review(1), review(2)]))
            .await;
        key(&mut app, KeyCode::Char('?')).await;
        key(&mut app, KeyCode::Down).await;
        key(&mut app, KeyCode::Tab).await;
        assert_eq!(app.selected_pr, 0);
        assert_eq!(app.active_tab, Tab::PendingReviews);
        key(&mut app, KeyCode::Esc).await;
        key(&mut app, KeyCode::Enter).await;
        key(&mut app, KeyCode::Tab).await;
        assert_eq!(app.active_tab, Tab::PendingReviews);
        key(&mut app, KeyCode::Esc).await;
        app.filter = "no match".into();
        app.update_filtered_indices();
        key(&mut app, KeyCode::Enter).await;
        assert!(!app.show_action_menu);
    }

    #[tokio::test]
    async fn home_end_follow_filtered_rows_and_refresh_bypasses_cache() {
        let mut app = App::new(config(), 30).await.unwrap();
        app.finish_load(
            Tab::PendingReviews,
            Ok(vec![review(10), review(20), review(21)]),
        )
        .await;
        app.filter = "feature 2".into();
        app.update_filtered_indices();
        key(&mut app, KeyCode::End).await;
        assert_eq!(app.selected_pr_item().unwrap().pr_number, 21);
        key(&mut app, KeyCode::Home).await;
        assert_eq!(app.selected_pr_item().unwrap().pr_number, 20);
        key(&mut app, KeyCode::Char('r')).await;
        assert!(app.loading);
        assert_eq!(app.loads.len(), 1);
        assert!(!key(&mut app, KeyCode::Char('q')).await);
    }
    #[tokio::test]
    async fn readiness_focus_captures_navigation_refresh_and_quit() {
        let mut app = App::new(config(), 0).await.unwrap();
        app.finish_load(Tab::PendingReviews, Ok(vec![review(1), review(2)]))
            .await;
        key(&mut app, KeyCode::Char('d')).await;
        assert!(app.show_readiness);
        key(&mut app, KeyCode::Down).await;
        key(&mut app, KeyCode::Tab).await;
        assert_eq!(app.selected_pr, 0);
        assert_eq!(app.readiness_scroll, 1);
        key(&mut app, KeyCode::Char('r')).await;
        assert!(app.loading);
        assert!(!key(&mut app, KeyCode::Char('q')).await);
        key(&mut app, KeyCode::Esc).await;
        assert!(!app.show_readiness);
    }
}
