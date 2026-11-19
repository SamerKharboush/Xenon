//! macOS permission checks for Accessibility and Screen Recording.
//!
//! Both checks are lightweight (no system dialogs); the onboarding page
//! links to the relevant System Settings panes so the user can grant access.

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

#[derive(serde::Serialize)]
pub struct PermissionStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

/// Report current macOS permission state.
#[tauri::command]
pub fn get_permissions() -> PermissionStatus {
    PermissionStatus {
        accessibility: ax_is_process_trusted(),
        screen_recording: screen_capture_granted(),
    }
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
    /// Returns 1 if Screen Recording access is preflighted, 0 otherwise.
    fn CGPreflightScreenCaptureAccess() -> bool;
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
            accessibility: true,
            screen_recording: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("accessibility"));
        assert!(json.contains("screen_recording"));
    }
}