//! Cross-platform desktop notifications and browser opening.
//!
//! We use the `notify-rust` crate which talks to native Cocoa APIs on
//! macOS (NSUserNotification) and D-Bus on Linux. No shelling out to
//! osascript — that was opening the Script Editor whenever the
//! AppleScript parser choked on special characters in a PR title.

/// Send a desktop notification (macOS + Linux).
pub(crate) async fn send_notification(title: &str, message: &str) {
    let title = title.to_string();
    let message = message.to_string();
    // Offload to a blocking thread — notify-rust is synchronous and
    // can take a while on first call (app-identity registration).
    tokio::task::spawn_blocking(move || {
        if let Err(e) = notify_rust::Notification::new()
            .summary(&title)
            .body(&message)
            .appname("pilot")
            .show()
        {
            tracing::warn!("Desktop notification failed: {e}");
        }
    });
}

/// Open a URL in the default browser (macOS + Linux).
pub(crate) fn open_url(url: &str) {
    let url = url.to_string();
    tokio::spawn(async move {
        #[cfg(target_os = "macos")]
        {
            let _ = tokio::process::Command::new("open")
                .arg(&url)
                .output()
                .await;
        }

        #[cfg(target_os = "linux")]
        {
            let _ = tokio::process::Command::new("xdg-open")
                .arg(&url)
                .output()
                .await;
        }
    });
}
