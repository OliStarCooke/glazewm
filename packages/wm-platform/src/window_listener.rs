use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;

use crate::{platform_impl, Dispatcher, WindowEvent, WindowId};

/// A listener for system-wide window events.
pub struct WindowListener {
  event_rx: mpsc::UnboundedReceiver<WindowEvent>,

  /// Coalesced backlog of received events.
  ///
  /// Populated by draining the channel on every `next_event` call, so
  /// that `LOCATIONCHANGE` floods collapse into a single handling (and
  /// a single `platform_sync`) per window.
  pending: VecDeque<WindowEvent>,

  /// Inner platform-specific window listener.
  inner: platform_impl::WindowListener,
}

impl WindowListener {
  /// Creates a new window listener.
  pub fn new(dispatcher: &Dispatcher) -> crate::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let inner = platform_impl::WindowListener::new(event_tx, dispatcher)?;

    Ok(Self {
      event_rx,
      pending: VecDeque::new(),
      inner,
    })
  }

  /// Returns the next window event from the listener.
  ///
  /// This will block until a window event is available. Any additional
  /// events already queued are drained and coalesced first (see
  /// `coalesce_moved_events`), so callers observe at most one
  /// non-interactive `MovedOrResized` event per window per batch.
  pub async fn next_event(&mut self) -> Option<WindowEvent> {
    if self.pending.is_empty() {
      self.pending.push_back(self.event_rx.recv().await?);
    }

    // Drain anything else already queued without waiting.
    while let Ok(event) = self.event_rx.try_recv() {
      self.pending.push_back(event);
    }

    coalesce_moved_events(&mut self.pending);
    self.pending.pop_front()
  }

  /// Terminates the window listener.
  pub fn terminate(&mut self) {
    self.inner.terminate();
  }
}

/// Drops superseded non-interactive `MovedOrResized` events, keeping only
/// the latest event per window.
///
/// Interactive drag boundaries (`is_interactive_start` and
/// `is_interactive_end`) are never dropped, and the relative order of
/// all surviving events is preserved.
fn coalesce_moved_events(queue: &mut VecDeque<WindowEvent>) {
  let mut remaining_by_window = HashMap::new();

  for event in queue.iter() {
    if let Some(window_id) = coalescible_window_id(event) {
      *remaining_by_window.entry(window_id).or_insert(0usize) += 1;
    }
  }

  if remaining_by_window.values().all(|&count| count < 2) {
    return;
  }

  let mut kept = VecDeque::with_capacity(queue.len());

  for event in queue.drain(..) {
    let is_superseded = match coalescible_window_id(&event) {
      Some(window_id) => {
        match remaining_by_window.get_mut(&window_id) {
          Some(remaining) => {
            *remaining -= 1;
            *remaining > 0
          }
          // Not counted above; keep the event.
          None => false,
        }
      }
      None => false,
    };

    if !is_superseded {
      kept.push_back(event);
    }
  }

  *queue = kept;
}

/// Gets the window ID if the event is a non-interactive `MovedOrResized`
/// event that is safe to coalesce.
fn coalescible_window_id(event: &WindowEvent) -> Option<WindowId> {
  match event {
    WindowEvent::MovedOrResized {
      window,
      is_interactive_start: false,
      is_interactive_end: false,
      ..
    } => Some(window.id()),
    _ => None,
  }
}
