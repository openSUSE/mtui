//! Best-effort desktop notifications for the interactive REPL.
//!
//! Ported from upstream `mtui/cli/notification.py` (+ `repl.py::notify_user`).
//! Desktop toasts are an opt-in courtesy compiled in behind the `notify`
//! feature (which pulls in [`notify-rust`]). When the feature is absent, or the
//! process is not attached to an interactive desktop session, [`display`]
//! degrades to a quiet no-op so headless, piped, cron, and `mtui-mcp` runs never
//! attempt to pop a toast.
//!
//! ## Headless guard
//!
//! A toast only makes sense when a user is sitting at an interactive terminal
//! with a graphical session, so [`display`] first checks
//! [`desktop_available`]: `stdin` must be a TTY, and on Linux/BSD a graphical
//! session (`DISPLAY` / `WAYLAND_DISPLAY`) must be present; macOS always
//! qualifies once the TTY check passes. The predicate is factored out and
//! parameterised (`desktop_available_with`) so it is unit-testable without a
//! real terminal or display.

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
/// * `stdin_is_tty` — is stdin attached to a terminal?
/// * `os` — the target OS string (`std::env::consts::OS`: `"macos"`, `"linux"`,
///   …).
/// * `has_env` — whether a named environment variable is set.
fn desktop_available_with(stdin_is_tty: bool, os: &str, has_env: impl Fn(&str) -> bool) -> bool {
    if !stdin_is_tty {
        return false;
    }
    if os == "macos" {
        return true;
    }
    // Linux/BSD: a freedesktop notification needs a graphical session.
    has_env("DISPLAY") || has_env("WAYLAND_DISPLAY")
}

/// Displays a best-effort desktop notification.
///
/// A no-op when [`desktop_available`] is false, and (without the `notify`
/// feature) always a no-op beyond the guard + a debug log. Failures from the
/// backend are swallowed and debug-logged — a notification must never break the
/// REPL.
///
/// `summary` is the title, `text` the body, `icon` an optional freedesktop icon
/// name (e.g. `"dialog-error"`).
fn display(summary: Option<&str>, text: Option<&str>, icon: Option<&str>) {
    if !desktop_available() {
        return;
    }
    // Only reached with a real desktop TTY, so the offline test suite exercises
    // the guard's `return` above but not this backend hop; `display_backend` is
    // covered directly in tests, and the guard→backend edge needs a pty harness
    // (out of scope — see module docs).
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

/// Maps upstream `repl.py::notify_user` onto [`display`]: a `"MTUI"`-titled
/// toast, using the freedesktop `dialog-error` icon for error-class messages.
///
/// A thin convenience for command code (e.g. the `update` start/finish toasts)
/// so callers don't repeat the title/icon convention.
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
        // In the test harness stdin is not a TTY, so `desktop_available` is
        // false and `display` must return without touching any backend. We can
        // only assert it does not panic / hang.
        display(Some("MTUI"), Some("hello"), None);
        notify_user("done", false);
        notify_user("boom", true);
    }

    #[test]
    fn backend_handles_all_field_combinations() {
        // Drive `display_backend` directly — the harness stdin is not a TTY, so
        // `display`'s guard would short-circuit before ever reaching it. Under
        // the default build (`notify` off) this exercises the no-op body; under
        // `--features notify` it builds the `Notification` and swallows the
        // `show()` error on a headless bus. Either way it must not panic across
        // the present/absent matrix of every optional field.
        display_backend(None, None, None);
        display_backend(Some("MTUI"), None, None);
        display_backend(Some("MTUI"), Some("body"), None);
        display_backend(Some("MTUI"), Some("body"), Some("dialog-error"));
    }

    #[test]
    fn desktop_available_reads_the_real_environment() {
        // Exercise the real (un-injected) entry point so the closure that reads
        // `std::env::var_os` and the `std::io::stdin().is_terminal()` probe are
        // covered. The result depends on the harness environment (typically not
        // a TTY → false), so we only assert it returns a bool without panicking
        // and agrees with the pure core on the same inputs.
        let real = desktop_available();
        let expected =
            desktop_available_with(std::io::stdin().is_terminal(), std::env::consts::OS, |k| {
                std::env::var_os(k).is_some()
            });
        assert_eq!(real, expected);
    }
}
