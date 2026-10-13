//! Desktop notifications through org.freedesktop.Notifications (noctalia, mako, …).

use std::collections::HashMap;

use zbus::zvariant::Value;

/// Sends a notification without blocking the UI (best effort).
pub fn send(summary: String, body: String) {
    std::thread::spawn(move || {
        if let Err(err) = try_send(&summary, &body) {
            log::debug!("notification failed: {err}");
        }
    });
}

fn try_send(summary: &str, body: &str) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "UltraDisplay",
            0u32,
            "video-display",
            summary,
            body,
            Vec::<&str>::new(),
            hints,
            5000i32,
        ),
    )?;
    Ok(())
}
