//! macOS permission checks for Accessibility, Screen Recording, and Microphone.
//!
//! Status is read from the OS (AXIsProcessTrusted, CGPreflightScreenCaptureAccess,
//! AVCaptureDevice authorizationStatusForMediaType:) — never from TCC files.
//! Requesting goes through the public APIs only:
//! - Screen Recording: CGRequestScreenCaptureAccess (CoreGraphics FFI).
//! - Microphone: AVCaptureDevice requestAccessForMediaType: (AVFoundation).
//! - Accessibility: AXIsProcessTrustedWithOptions with the prompt option.
//!
//! Input Monitoring is NOT queried or requested: it is reported as a static
//! "not-needed" entry. The cursor is read via `NSEvent::mouseLocation()`
//! (see `cursor.rs`), which needs no TCC grant, and even a ListenEvent
//! *preflight* would emit a kTCCServiceListenEvent request in the TCC log.
//!
//! The onboarding page and Settings → Permissions surface the real status and
//! link to the matching System Settings panes. Nothing here writes TCC.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{AVCaptureDevice, AVMediaType};
use objc2_foundation::{ns_string, NSMutableDictionary, NSNumber, NSString};

/// Check whether this process has been granted Accessibility access
/// (AXIsProcessTrusted from ApplicationServices).
fn ax_is_process_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Check whether Screen Recording permission has been granted
/// (CGPreflightScreenCaptureAccess from CoreGraphics).
fn screen_capture_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Input Monitoring is NOT required: the cursor is read via
/// `NSEvent::mouseLocation()` (see `cursor.rs`), which needs no TCC grant.
/// We deliberately do NOT call `CGPreflightListenEventAccess()` — even a
/// preflight generates a `kTCCServiceListenEvent` request in the TCC log, which
/// would re-introduce the prompt this change removes. The status line is a
/// static informational "not needed".
fn input_monitoring_entry() -> PermissionEntry {
    PermissionEntry {
        granted: false,
        state: "not-needed",
        requestable: false,
    }
}

/// One permission's status: `granted` (bool from the OS), a short `state`
/// string for the UI ("granted" / "denied" / "not-determined"), and whether
/// this build can request it in-app (`requestable`).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionEntry {
    pub granted: bool,
    /// "granted" | "denied" | "not-determined" | "restricted"
    pub state: &'static str,
    /// True when an in-app "Request" button makes sense for this permission.
    pub requestable: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionStatus {
    pub accessibility: PermissionEntry,
    pub screen_recording: PermissionEntry,
    pub input_monitoring: PermissionEntry,
    pub microphone: PermissionEntry,
}

fn state_str(granted: bool) -> &'static str {
    if granted { "granted" } else { "denied" }
}

/// Map an AVFoundation AVAuthorizationStatus (i32) to (granted, state).
/// 0 = not-determined, 1 = restricted, 2 = denied, 3 = authorized.
fn mic_status_from_av(status: isize) -> (bool, &'static str) {
    match status {
        3 => (true, "granted"),
        0 => (false, "not-determined"),
        1 => (false, "restricted"),
        _ => (false, "denied"),
    }
}

/// AVMediaTypeAudio is the Objective-C string constant "soun" (AVMediaType is
/// a type alias of NSString). Build it from the literal to avoid depending on
/// the optional static.
fn audio_media_type() -> Retained<AVMediaType> {
    NSString::from_str("soun")
}
/// Returns `(granted, state, raw_status)`. On any failure, returns a
/// conservative "denied" so the UI still shows a Request button.
fn microphone_status() -> (bool, &'static str, isize) {
    let status = unsafe {
        let mt = audio_media_type();
        AVCaptureDevice::authorizationStatusForMediaType(&mt).0
    };
    let (granted, state) = if status < 0 {
        (false, "denied")
    } else {
        mic_status_from_av(status)
    };
    (granted, state, status)
}

/// Report current macOS permission state.
#[tauri::command]
pub fn get_permissions() -> PermissionStatus {
    let ax = ax_is_process_trusted();
    let screen = screen_capture_granted();
    let (mic, mic_state, _raw) = microphone_status();
    PermissionStatus {
        accessibility: PermissionEntry {
            granted: ax,
            state: state_str(ax),
            // Accessibility can only be granted in System Settings, so the
            // in-app action is the "Open Settings" deep link, not a prompt.
            requestable: false,
        },
        screen_recording: PermissionEntry {
            granted: screen,
            state: state_str(screen),
            requestable: true,
        },
        input_monitoring: input_monitoring_entry(),
        microphone: PermissionEntry {
            granted: mic,
            state: mic_state,
            requestable: true,
        },
    }
}

/// Request a permission in-app where the OS allows it.
///
/// * `microphone` — AVCaptureDevice requestAccessForMediaType: (shows the
///   system prompt the first time; silent re-check afterwards).
/// * `screen` — CGRequestScreenCaptureAccess (CoreGraphics FFI).
/// * `accessibility` — AXIsProcessTrustedWithOptions with the prompt option,
///   which opens the Accessibility pane.
///
/// Never touches TCC directly. Returns the fresh status for that permission so
/// the caller can update the UI without a second round-trip.
#[tauri::command]
pub fn request_permission(area: String) -> Result<PermissionEntry, String> {
    match area.as_str() {
        "microphone" => request_microphone(),
        "screen" => {
            unsafe { CGRequestScreenCaptureAccess() };
            let granted = screen_capture_granted();
            Ok(PermissionEntry { granted, state: state_str(granted), requestable: true })
        }
        "input" => {
            // Input Monitoring is NOT required (cursor via NSEvent::mouseLocation).
            // Never call CGRequestListenEventAccess or CGPreflightListenEventAccess;
            // report the static "not needed" entry.
            Ok(input_monitoring_entry())
        }
        "accessibility" => {
            // Prompt the user via the trusted-options API; it opens the
            // Accessibility pane when not yet trusted.
            unsafe {
                let dict = NSMutableDictionary::<NSString, AnyObject>::new();
                dict.insert(
                    ns_string!("AXTrustedCheckOptionPrompt"),
                    &NSNumber::new_bool(true),
                );
                AXIsProcessTrustedWithOptions(&*dict as *const _ as *const _);
            }
            let granted = ax_is_process_trusted();
            Ok(PermissionEntry { granted, state: state_str(granted), requestable: false })
        }
        other => Err(format!("unknown permission area: {other}")),
    }
}

/// AVFoundation Microphone request, blocking on the completion handler.
fn request_microphone() -> Result<PermissionEntry, String> {
    // Nothing to prompt for if already decided.
    let (_granted, _state, status) = microphone_status();
    if status != 0 {
        let (granted, state, _) = microphone_status();
        return Ok(PermissionEntry { granted, state, requestable: true });
    }

    let mt = audio_media_type();
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let handler = block2::StackBlock::new(move |granted: objc2::runtime::Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(&mt, &handler);
    }
    rx.recv_timeout(std::time::Duration::from_secs(120))
        .map_err(|e| format!("microphone request timed out: {e}"))?;
    // Re-read the authoritative status after the prompt.
    let (granted, state, _) = microphone_status();
    Ok(PermissionEntry { granted, state, requestable: true })
}

/// Open the System Settings pane for the given permission area.
/// `area` is "accessibility", "screen", or "microphone".
#[tauri::command]
pub fn open_permission_settings(area: String) -> Result<(), String> {
    let url = match area.as_str() {
        "accessibility" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        "screen" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        "input" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        }
        "microphone" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        other => return Err(format!("unknown permission area: {other}")),
    };
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- FFI bindings -----------------------------------------------------------

extern "C" {
    /// Returns 1 if the process has Accessibility trust, 0 otherwise.
    fn AXIsProcessTrusted() -> i32;
    /// Screen Recording preflight (no prompt).
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// Screen Recording request (shows the system prompt / adds the app row).
    fn CGRequestScreenCaptureAccess();
    /// Input Monitoring is intentionally not queried: even a preflight
    /// (`CGPreflightListenEventAccess`) emits a kTCCServiceListenEvent request.
    /// Cursor location uses NSEvent, which needs no grant.
    /// AXIsProcessTrustedWithOptions: options dict with the prompt key.
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

// Link against the macOS frameworks.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_status_is_serializable() {
        let status = PermissionStatus {
            accessibility: PermissionEntry { granted: true, state: "granted", requestable: false },
            screen_recording: PermissionEntry { granted: false, state: "denied", requestable: true },
            input_monitoring: PermissionEntry { granted: false, state: "not-needed", requestable: false },
            microphone: PermissionEntry { granted: false, state: "not-determined", requestable: true },
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("accessibility"));
        assert!(json.contains("screenRecording"));
        assert!(json.contains("inputMonitoring"));
        assert!(json.contains("microphone"));
        assert!(json.contains("not-determined"));
    }

    #[test]
    fn av_status_mapping_is_correct() {
        assert_eq!(mic_status_from_av(3), (true, "granted"));
        assert_eq!(mic_status_from_av(0), (false, "not-determined"));
        assert_eq!(mic_status_from_av(1), (false, "restricted"));
        assert_eq!(mic_status_from_av(2), (false, "denied"));
    }

    /// The real get_permissions() runs against the OS; on the test host it must
    /// at least not panic and must return a consistent shape.
    #[test]
    fn real_status_has_all_permissions() {
        let s = get_permissions();
        let _ = s.accessibility.granted;
        let _ = s.screen_recording.granted;
        let _ = s.input_monitoring.granted;
        let _ = s.microphone.granted;
        assert!(!s.accessibility.state.is_empty());
        assert!(!s.screen_recording.state.is_empty());
        assert!(!s.input_monitoring.state.is_empty());
        assert_eq!(s.input_monitoring.state, "not-needed");
        assert!(!s.microphone.state.is_empty());
    }
}