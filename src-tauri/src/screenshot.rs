//! Perception: capture the monitor under the cursor (or the primary one),
//! downscale it to fit a 512×512 budget, and map model-returned image
//! coordinates back to logical screen points.
//!
//! Coordinate model (macOS):
//! - `xcap` captures **physical** pixels (Retina = 2× logical points).
//! - `enigo` clicks in **logical** points.
//! - The model sees a **downscaled** image (aspect preserved, no letterbox).
//!
//! So the only conversion that matters is image px -> logical pt, which is a
//! per-axis multiply by `logical_size / image_size`. Physical pixels are used
//! only to size the image, never to place the mouse.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;
use xcap::Monitor;

/// Longest edge of the image handed to the vision model (PRD §7.2).
pub const MAX_SIDE: u32 = 512;

/// A captured, downscaled screen image plus the geometry needed to map a
/// model-returned point back onto the logical screen.
#[derive(Debug, Clone)]
pub struct Screenshot {
    /// PNG bytes of the downscaled image (what the model actually sees).
    image: Vec<u8>,
    /// Image dimensions of the downscaled PNG.
    image_width: u32,
    image_height: u32,
    /// Logical size of the monitor, in points.
    logical_width: f64,
    logical_height: f64,
    /// Logical origin of the monitor (multi-monitor offsets).
    origin_x: f64,
    origin_y: f64,
}

impl Screenshot {
    pub fn image_width(&self) -> u32 {
        self.image_width
    }

    pub fn image_height(&self) -> u32 {
        self.image_height
    }

    /// OpenAI-format data URI for the image (`data:image/png;base64,...`).
    pub fn data_uri(&self) -> String {
        format!("data:image/png;base64,{}", BASE64.encode(&self.image))
    }

    /// Map an image-pixel point to logical screen coordinates (what `enigo`
    /// expects). Aspect ratio is preserved by construction, so each axis is
    /// scaled independently.
    pub fn map_point(&self, x: f64, y: f64) -> (i32, i32) {
        map_image_to_screen(
            (x, y),
            (self.image_width, self.image_height),
            (self.logical_width, self.logical_height),
            (self.origin_x, self.origin_y),
        )
    }

    /// Normalize an image-pixel point to 0..1 (for the `agent-focus` event).
    pub fn focus_point(&self, x: f64, y: f64) -> (f64, f64) {
        (
            (x / self.image_width as f64).clamp(0.0, 1.0),
            (y / self.image_height as f64).clamp(0.0, 1.0),
        )
    }
}

/// Pure coordinate mapping, kept separate so it can be unit-tested without a
/// display. `point` is in image pixels; `logical_size`/`origin` describe the
/// monitor's logical (point) space.
pub fn map_image_to_screen(
    point: (f64, f64),
    image_size: (u32, u32),
    logical_size: (f64, f64),
    origin: (f64, f64),
) -> (i32, i32) {
    let sx = origin.0 + point.0 * (logical_size.0 / image_size.0 as f64);
    let sy = origin.1 + point.1 * (logical_size.1 / image_size.1 as f64);
    (sx.round() as i32, sy.round() as i32)
}

/// Longest edge after fitting `w`×`h` into `max_side` without upscaling.
pub fn fit_downscale(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let factor = (max_side as f64 / w as f64)
        .min(max_side as f64 / h as f64)
        .min(1.0);
    (
        ((w as f64 * factor).round() as u32).max(1),
        ((h as f64 * factor).round() as u32).max(1),
    )
}

/// Capture the monitor under the cursor, or the primary monitor when the
/// cursor cannot be located.
pub fn capture() -> Result<Screenshot> {
    maybe_request_screen_recording();
    capture_monitor(None)
}

/// On the agent's first screenshot, ask macOS for Screen Recording if it has
/// not been granted yet (CGPreflight/CGRequestScreenCaptureAccess). This is the
/// only place the app triggers the system prompt outside onboarding/Settings.
/// Guarded by a process-wide one-shot flag; never touches TCC directly.
fn maybe_request_screen_recording() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ASKED: AtomicBool = AtomicBool::new(false);
    if ASKED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            CGRequestScreenCaptureAccess();
        }
    }
}

extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess();
}

/// Capture a specific monitor by index, or the cursor/primary monitor when
/// `index` is `None`.
pub fn capture_monitor(index: Option<usize>) -> Result<Screenshot> {
    let monitors = Monitor::all().map_err(|e| anyhow!("screen capture failed: {e}"))?;
    let monitor = pick_monitor(index, &monitors)?;
    build_screenshot(&monitor)
}

fn build_screenshot(monitor: &Monitor) -> Result<Screenshot> {
    let raw = monitor
        .capture_image()
        .map_err(|e| anyhow!("screen capture failed: {e}"))?;
    let physical_width = raw.width();
    let physical_height = raw.height();

    let logical_width = monitor.width().map_err(|e| anyhow!("monitor size: {e}"))? as f64;
    let logical_height = monitor.height().map_err(|e| anyhow!("monitor size: {e}"))? as f64;
    let origin_x = monitor.x().map_err(|e| anyhow!("monitor origin: {e}"))? as f64;
    let origin_y = monitor.y().map_err(|e| anyhow!("monitor origin: {e}"))? as f64;

    let (image_width, image_height) = fit_downscale(physical_width, physical_height, MAX_SIDE);
    let resized = downscale(&raw, image_width, image_height);
    let encoded = encode_png(&resized)?;

    Ok(Screenshot {
        image: encoded,
        image_width,
        image_height,
        logical_width,
        logical_height,
        origin_x,
        origin_y,
    })
}

fn downscale(img: &RgbaImage, width: u32, height: u32) -> DynamicImage {
    if width == img.width() && height == img.height() {
        DynamicImage::ImageRgba8(img.clone())
    } else {
        DynamicImage::ImageRgba8(image::imageops::resize(img, width, height, image::imageops::FilterType::Lanczos3))
    }
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

fn pick_monitor(index: Option<usize>, monitors: &[Monitor]) -> Result<Monitor> {
    if let Some(i) = index {
        return monitors
            .get(i)
            .cloned()
            .ok_or_else(|| anyhow!("monitor {} not found", i));
    }
    if let Some((x, y)) = cursor_point() {
        if let Ok(m) = Monitor::from_point(x, y) {
            return Ok(m);
        }
    }
    monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .cloned()
        .ok_or_else(|| anyhow!("no primary monitor found"))
}

/// Cursor position in logical points (top-left global space), via
/// `NSEvent::mouseLocation()` — permission-free and thread-safe (unlike the old
/// CGEventSource, which triggered the Input Monitoring prompt).
fn cursor_point() -> Option<(i32, i32)> {
    crate::cursor::location().map(|(x, y)| (x as i32, y as i32))
}

/// Test-only constructor for a Screenshot with a real image (512×320).
#[cfg(test)]
impl Screenshot {
    pub(crate) fn test_screenshot(iw: u32, ih: u32, lw: f64, lh: f64) -> Screenshot {
        let img = DynamicImage::new_rgba8(iw, ih);
        let encoded = encode_png(&img).unwrap();
        Screenshot {
            image: encoded,
            image_width: iw,
            image_height: ih,
            logical_width: lw,
            logical_height: lh,
            origin_x: 0.0,
            origin_y: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_downscale_preserves_aspect_and_caps_long_edge() {
        // 2880x1800 Retina -> 512x320 (1.8x factor).
        assert_eq!(fit_downscale(2880, 1800, 512), (512, 320));
        // Already small: no upscale.
        assert_eq!(fit_downscale(400, 300, 512), (400, 300));
        // Square.
        assert_eq!(fit_downscale(2560, 2560, 512), (512, 512));
    }

    #[test]
    fn map_retina_half_point_is_center() {
        // Retina 2x: 2880x1800 physical = 1440x900 logical, no origin.
        // Image downscaled to 512x320. Model point at image center
        // (256,160) -> logical center (720,450).
        let (x, y) = map_image_to_screen(
            (256.0, 160.0),
            (512, 320),
            (1440.0, 900.0),
            (0.0, 0.0),
        );
        assert_eq!((x, y), (720, 450));
    }

    #[test]
    fn map_non_square_screen() {
        // 1000x500 logical image 250x125: (100,50) -> (400,200).
        let (x, y) = map_image_to_screen((100.0, 50.0), (250, 125), (1000.0, 500.0), (0.0, 0.0));
        assert_eq!((x, y), (400, 200));
    }

    #[test]
    fn map_adds_monitor_origin() {
        // Secondary monitor offset by (1440, 0): image center -> that origin.
        let (x, y) = map_image_to_screen(
            (256.0, 160.0),
            (512, 320),
            (1440.0, 900.0),
            (1440.0, 0.0),
        );
        assert_eq!((x, y), (2160, 450));
    }

    #[test]
    fn map_scales_linearly_past_edge() {
        // Mapping is a pure multiply; out-of-image model coords stay proportional.
        let (x, y) = map_image_to_screen((600.0, 400.0), (512, 320), (1440.0, 900.0), (0.0, 0.0));
        assert_eq!((x, y), (1688, 1125));
    }

    #[test]
    fn data_uri_is_base64_png() {
        let shot = Screenshot::test_screenshot(512, 320, 1440.0, 900.0);
        let uri = shot.data_uri();
        assert!(uri.starts_with("data:image/png;base64,"));
        assert!(!uri[uri.len() - 1..].is_empty());
    }

    #[test]
    fn focus_point_normalizes() {
        let shot = Screenshot::test_screenshot(512, 320, 1440.0, 900.0);
        let (fx, fy) = shot.focus_point(256.0, 160.0);
        assert!((fx - 0.5).abs() < 1e-9);
        assert!((fy - 0.5).abs() < 1e-9);
        let (cx, cy) = shot.focus_point(0.0, 0.0);
        assert_eq!((cx, cy), (0.0, 0.0));
    }
}