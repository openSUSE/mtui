//! Best-effort desktop notifications for the interactive REPL.
//!
//! Toasts are an opt-in courtesy behind the `notify` feature (`notify-rust`).
//! Without the feature, or off an interactive desktop session, `display` is a
//! quiet no-op, so headless, piped, cron and `mtui-mcp` runs never pop one.
//!
//! The guard is `desktop_available`: `stdin` must be a TTY, and on Linux/BSD a
//! graphical session (`DISPLAY` / `WAYLAND_DISPLAY`) must be present; macOS
//! qualifies on the TTY check alone. It is parameterised
//! (`desktop_available_with`) so it is unit-testable without a real terminal.

use std::io::IsTerminal;

/// Reports whether a desktop notification can plausibly be shown in the current
/// process environment (real `stdin` TTY + platform display checks).
#[must_use]
fn desktop_available() -> bool {
    desktop_available_with(std::io::stdin().is_terminal(), std::env::consts::OS, |k| {
        std::env::var_os(k).is_some()
    })
}

/// The pure core of [`desktop_available`], with the environment injected.
///
/// `os` is the `std::env::consts::OS` string; `has_env` reports whether a named
/// environment variable is set.
fn desktop_available_with(stdin_is_tty: bool, os: &str, has_env: impl Fn(&str) -> bool) -> bool {
    if !stdin_is_tty {
        return false;
    }
    if os == "macos" {
        return true;
    }
    // A freedesktop notification needs a graphical session.
    has_env("DISPLAY") || has_env("WAYLAND_DISPLAY")
}

/// Displays a best-effort desktop notification.
///
/// A no-op when `desktop_available` is false, and without the `notify` feature
/// a no-op beyond a debug log. Backend failures are swallowed and debug-logged:
/// a notification must never break the REPL.
///
/// `summary` is the title, `text` the body, `icon` an optional freedesktop icon
/// name.
fn display(summary: Option<&str>, text: Option<&str>, icon: Option<&str>) {
    if !desktop_available() {
        return;
    }
    // Only reached with a real desktop TTY, so the offline suite covers
    // `display_backend` directly; the guard→backend edge needs a pty harness.
    display_backend(summary, text, icon);
}

#[cfg(feature = "notify")]
fn display_backend(summary: Option<&str>, text: Option<&str>, icon: Option<&str>) {
    tracing::debug!(?text, "displaying desktop notification");
    let mut n = notify_rust::Notification::new();
    n.appname("mtui");
    if let Some(s) = summary {
        n.summary(s);
    }
    if let Some(t) = text {
        n.body(t);
    }
    if let Some(i) = icon {
        n.icon(i);
    }
    if let Err(e) = n.show() {
        tracing::debug!("failed to display notification: {e}");
    }
}

#[cfg(not(feature = "notify"))]
fn display_backend(_summary: Option<&str>, _text: Option<&str>, _icon: Option<&str>) {
    tracing::debug!("notify feature disabled; skipping desktop notification");
}

/// Shows a `"MTUI"`-titled toast via `display`, using the freedesktop
/// `dialog-error` icon for error-class messages, so callers need not repeat the
/// title/icon convention.
pub fn notify_user(msg: &str, error: bool) {
    let icon = if error { Some("dialog-error") } else { None };
    display(Some("MTUI"), Some(msg), icon);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_tty_is_never_available() {
        assert!(!desktop_available_with(false, "macos", |_| true));
        assert!(!desktop_available_with(false, "linux", |_| true));
    }

    #[test]
    fn macos_tty_is_always_available() {
        assert!(desktop_available_with(true, "macos", |_| false));
    }

    #[test]
    fn linux_tty_needs_a_display() {
        assert!(!desktop_available_with(true, "linux", |_| false));
        assert!(desktop_available_with(true, "linux", |k| k == "DISPLAY"));
        assert!(desktop_available_with(true, "linux", |k| k == "WAYLAND_DISPLAY"));
    }

    #[test]
    fn display_is_a_noop_when_headless() {
        // The harness stdin is not a TTY, so `display` returns before any
        // backend hop; only "does not panic" is assertable.
        display(Some("MTUI"), Some("hello"), None);
        notify_user("done", false);
        notify_user("boom", true);
    }

    #[test]
    fn backend_handles_all_field_combinations() {
        // Driven directly, since `display`'s guard short-circuits in the
        // harness. Neither the `notify`-off no-op body nor the real
        // `Notification` (whose `show()` fails on a headless bus) may panic
        // across the present/absent matrix of every optional field.
        display_backend(None, None, None);
        display_backend(Some("MTUI"), None, None);
        display_backend(Some("MTUI"), Some("body"), None);
        display_backend(Some("MTUI"), Some("body"), Some("dialog-error"));
    }

    #[test]
    // Reads the process-global environment, so it joins the crate's one `env`
    // exclusion domain rather than racing another test's `set_var`.
    #[serial_test::serial(env)]
    fn desktop_available_reads_the_real_environment() {
        // The un-injected entry point, covering the `var_os` closure and the
        // `is_terminal` probe. Its value is harness-dependent, so all that can
        // be asserted is agreement with the pure core on the same inputs.
        let real = desktop_available();
        let expected =
            desktop_available_with(std::io::stdin().is_terminal(), std::env::consts::OS, |k| {
                std::env::var_os(k).is_some()
            });
        assert_eq!(real, expected);
    }
}
