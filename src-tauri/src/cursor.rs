//! Permission-free cursor location.
//!
//! The old approach read the cursor with `CGEventSource` + `CGEvent::new`.
//! Merely constructing a `CGEventSource` triggers the `kTCCServiceListenEvent`
//! (Input Monitoring) TCC check, so users were prompted to grant Input
//! Monitoring just to know where the mouse is. `NSEvent::mouseLocation()` needs
//! no permission, is safe to call off the main thread, and is cheap to poll.
//!
//! Cocoa's global coordinate space has its origin at the *bottom-left* of the
//! primary screen, while the hit-test / screenshot code expects the *top-left*
//! space (same as `CGEvent.location`). `cocoa_to_topleft` flips Y using the
//! primary screen height, cached once at startup (`NSScreen` is main-thread
//! only; `mouseLocation` is not).

use std::sync::atomic::{AtomicI32, Ordering};

/// Primary screen height in points, captured at startup. `0` until set.
static PRIMARY_HEIGHT: AtomicI32 = AtomicI32::new(0);

/// Convert a Cocoa point (origin bottom-left of the primary screen, points) to
/// the top-left global space the hit-test and screenshot code expect.
///
/// `height` is the primary screen height in points. X is unchanged; Y is
/// mirrored: `y_topleft = height - y_cocoa`.
pub fn cocoa_to_topleft(x: f64, y: f64, height: f64) -> (f64, f64) {
    (x, height - y)
}

/// Record the primary screen height (in points). Call once on the main thread.
pub fn set_primary_height(height: f64) {
    PRIMARY_HEIGHT.store(height as i32, Ordering::Relaxed);
}

/// Current cursor position in top-left global points, or `None` if the primary
/// screen height is not known yet (or AppKit is unavailable).
///
/// Thread-safe and permission-free.
pub fn location() -> Option<(f64, f64)> {
    let h = PRIMARY_HEIGHT.load(Ordering::Relaxed);
    if h <= 0 {
        return None;
    }
    let pt = objc2_app_kit::NSEvent::mouseLocation();
    Some(cocoa_to_topleft(pt.x, pt.y, h as f64))
}

/// Capture the primary screen height. Must be called on the main thread
/// (`NSScreen` is main-thread-only). Safe to call more than once.
pub fn init_primary_screen_height() {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let screens = objc2_app_kit::NSScreen::screens(mtm);
    // screens[0] is the primary screen (Cocoa global origin sits at its
    // bottom-left).
    if let Some(primary) = screens.firstObject() {
        let frame = primary.frame();
        set_primary_height(frame.size.height);
    }
}

#[cfg(test)]
mod tests {
    use super::cocoa_to_topleft;

    #[test]
    fn flips_y_around_primary_height() {
        // Bottom-left corner (y=0) -> top row (y=height).
        assert_eq!(cocoa_to_topleft(100.0, 0.0, 800.0), (100.0, 800.0));
        // Top-left corner (y=height) -> top row y=0.
        assert_eq!(cocoa_to_topleft(100.0, 800.0, 800.0), (100.0, 0.0));
        // Vertical middle is its own mirror.
        assert_eq!(cocoa_to_topleft(100.0, 400.0, 800.0), (100.0, 400.0));
    }

    #[test]
    fn keeps_x() {
        assert_eq!(cocoa_to_topleft(1920.0, 1080.0, 1080.0), (1920.0, 0.0));
    }
}