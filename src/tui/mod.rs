//! TUI module for PRCtrl.
//!
//! Provides a terminal user interface with:
//! - Left sidebar with command list
//! - Main area with PR list
//! - Auto-refresh mechanism
//! - Keyboard navigation

pub mod app;
pub mod events;
pub mod run;
pub mod ui;

pub use app::App;
pub use events::Event;
pub use run::run_tui;
pub use ui::Ui;
