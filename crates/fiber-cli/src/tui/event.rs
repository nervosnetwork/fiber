//! Event handling for the TUI.
//!
//! Manages keyboard input events via crossterm's event stream,
//! combined with periodic tick events for data refresh.

use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};

/// Application events.
#[derive(Debug)]
pub enum Event {
    /// A key was pressed.
    Key(KeyEvent),
    /// Periodic tick for data refresh.
    Tick,
    /// Terminal was resized.
    Resize,
}

/// Event handler that polls crossterm events with a tick interval.
pub struct EventHandler {
    tick_rate: Duration,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        Self { tick_rate }
    }

    /// Poll for the next event. Returns None if no event is available within the tick rate.
    pub fn poll_event(&self) -> anyhow::Result<Event> {
        if event::poll(self.tick_rate)? {
            match event::read()? {
                CrosstermEvent::Key(key) => Ok(Event::Key(key)),
                CrosstermEvent::Resize(_, _) => Ok(Event::Resize),
                _ => Ok(Event::Tick),
            }
        } else {
            Ok(Event::Tick)
        }
    }
}
