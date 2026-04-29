//! Animation lifecycle for the dashboard's modal slot.
//!
//! Exactly one chrome-driven modal (Send / Receive / Swap) is on screen at a
//! time, so animation state lives here rather than in any one modal — that way
//! a modal's `update` doesn't need to know about its own closing transition,
//! and the `view` for any modal can just plumb `chrome.progress()` into
//! `kao_widgets::modal_wrapper`. The account dropdown bypasses chrome entirely
//! (instant open/close).

use std::time::Instant;

const OPEN_MS: u128 = 220;
const CLOSE_MS: u128 = 220;

#[derive(Debug)]
pub struct ModalChrome {
    start: Instant,
    closing: bool,
    open: bool,
}

impl ModalChrome {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            closing: false,
            open: false,
        }
    }

    /// Begin (or restart) the open transition. Caller is opening a fresh
    /// modal — ignore any prior closing state.
    pub fn open(&mut self) {
        self.start = Instant::now();
        self.closing = false;
        self.open = true;
    }

    /// Start the close transition. No-op if no modal is open or if a close is
    /// already in flight.
    pub fn start_close(&mut self) {
        if !self.open || self.closing {
            return;
        }
        self.closing = true;
        self.start = Instant::now();
    }

    /// Eased 0..1 progress. 1.0 = fully open; 0.0 = fully closed. Returns 1.0
    /// when no modal is being driven so a stale read can't accidentally hide
    /// the box during a non-animated render.
    pub fn progress(&self) -> f32 {
        if !self.open {
            return 1.0;
        }
        let elapsed_ms = self.start.elapsed().as_millis();
        if self.closing {
            if elapsed_ms >= CLOSE_MS {
                return 0.0;
            }
            // ease-in: progress 1 -> 0, slow start, faster finish.
            let t = elapsed_ms as f32 / CLOSE_MS as f32;
            1.0 - t * t
        } else {
            if elapsed_ms >= OPEN_MS {
                return 1.0;
            }
            // ease-out cubic: snappy then settles.
            let t = elapsed_ms as f32 / OPEN_MS as f32;
            1.0 - (1.0 - t).powi(3)
        }
    }

    /// True while we still need per-frame ticks. Returns true during the open
    /// transition until the open easing completes, and stays true through the
    /// entire close transition so the cleanup `tick_settled` call lands.
    pub fn is_animating(&self) -> bool {
        if !self.open {
            return false;
        }
        if self.closing {
            return true;
        }
        self.start.elapsed().as_millis() < OPEN_MS
    }

    /// Call from a `Tick` handler. Returns true once the close transition has
    /// elapsed and resets internal state — caller should clear its modal slot
    /// and reset any per-modal state on `true`.
    pub fn tick_settled(&mut self) -> bool {
        if self.closing && self.start.elapsed().as_millis() >= CLOSE_MS {
            self.closing = false;
            self.open = false;
            return true;
        }
        false
    }
}

impl Default for ModalChrome {
    fn default() -> Self {
        Self::new()
    }
}
