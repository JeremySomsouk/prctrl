use std::time::Duration;
use tokio::sync::mpsc;
use crossterm::event::KeyEvent;

/// Custom event type for the application
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// Keyboard input
    Key(KeyEvent),
    /// Timer tick (for refresh and other periodic updates)
    Tick,
    /// Error occurred
    Error,
    /// Quit the application
    Quit,
}

/// Start the tick generator for periodic refresh
pub fn start_tick_generator(interval: Duration) -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel(100);
    
    tokio::spawn(async move {
        let mut interval_stream = tokio::time::interval(interval);
        
        loop {
            interval_stream.tick().await;
            if let Err(e) = sender.send(Event::Tick).await {
                eprintln!("Failed to send tick event: {}", e);
                break;
            }
        }
    });
    
    receiver
}
