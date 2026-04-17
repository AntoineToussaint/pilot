//! Cross-platform desktop notifications and browser opening.

/// Send a desktop notification (macOS + Linux).
pub(crate) async fn send_notification(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(message),
            applescript_string(title),
        );
        let _ = tokio::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .await;
    }

    #[cfg(target_os = "linux")]
    {
        let _ = tokio::process::Command::new("notify-send")
            .args([title, message])
            .output()
            .await;
    }
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

/// Escape a string for AppleScript.
#[cfg(target_os = "macos")]
fn applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
