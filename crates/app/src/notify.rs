/// Send a macOS notification safely (no command injection).
pub(crate) async fn send_macos_notification(title: &str, message: &str) {
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

/// Escape a string for AppleScript (wrap in quotes, escape backslash and quote).
fn applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
