from unittest.mock import MagicMock

import pytest

from mtui.cli import notification


@pytest.fixture(autouse=True)
def _reset_resolver(monkeypatch):
    """Each test starts from a clean, un-attempted resolver state."""
    monkeypatch.setattr(notification, "_notify_cls", None)
    monkeypatch.setattr(notification, "_resolved", False)


def test_display_success(monkeypatch):
    """A title/message are set and the notification is sent."""
    monkeypatch.setattr(notification, "_desktop_available", lambda: True)
    notify_instance = MagicMock()
    notify_cls = MagicMock(return_value=notify_instance)
    # Pretend notify-py resolved to our fake Notify class.
    monkeypatch.setattr(notification, "_notify_cls", notify_cls)
    monkeypatch.setattr(notification, "_resolved", True)

    notification.display("test summary", "test text")

    notify_cls.assert_called_once_with(default_application_name="mtui")
    assert notify_instance.title == "test summary"
    assert notify_instance.message == "test text"
    notify_instance.send.assert_called_once_with(block=False)


def test_display_sets_icon_when_given(monkeypatch):
    """An explicit icon is forwarded; otherwise it is left untouched."""
    monkeypatch.setattr(notification, "_desktop_available", lambda: True)
    notify_instance = MagicMock()
    notify_cls = MagicMock(return_value=notify_instance)
    monkeypatch.setattr(notification, "_notify_cls", notify_cls)
    monkeypatch.setattr(notification, "_resolved", True)

    notification.display("s", "t", "dialog-error")

    assert notify_instance.icon == "dialog-error"


def test_display_import_error(monkeypatch):
    """A missing notify-py extra is a quiet no-op, not an exception."""
    monkeypatch.setattr(notification, "_desktop_available", lambda: True)
    # Force the resolver to report notify-py as unavailable.
    monkeypatch.setattr(notification, "_resolve", lambda: None)

    # Should execute quietly without raising.
    notification.display("test summary", "test text")


def test_display_skipped_when_not_interactive(monkeypatch):
    """Without an interactive desktop session, nothing is constructed."""
    monkeypatch.setattr(notification, "_desktop_available", lambda: False)
    notify_cls = MagicMock()
    monkeypatch.setattr(notification, "_notify_cls", notify_cls)
    monkeypatch.setattr(notification, "_resolved", True)

    notification.display("test summary", "test text")

    notify_cls.assert_not_called()


def test_display_send_failure_swallowed(monkeypatch):
    """A backend failure during send must never propagate."""
    monkeypatch.setattr(notification, "_desktop_available", lambda: True)
    notify_instance = MagicMock()
    notify_instance.send.side_effect = RuntimeError("dbus exploded")
    notify_cls = MagicMock(return_value=notify_instance)
    monkeypatch.setattr(notification, "_notify_cls", notify_cls)
    monkeypatch.setattr(notification, "_resolved", True)

    # Should swallow the error.
    notification.display("test summary", "test text")

    notify_instance.send.assert_called_once_with(block=False)


def test_desktop_available_requires_tty(monkeypatch):
    """Non-tty stdin disables notifications regardless of platform."""
    monkeypatch.setattr(notification.sys.stdin, "isatty", lambda: False)
    assert notification._desktop_available() is False


def test_desktop_available_linux_needs_display(monkeypatch):
    """On Linux a graphical session (DISPLAY/WAYLAND) is required."""
    monkeypatch.setattr(notification.sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr(notification.sys, "platform", "linux")
    monkeypatch.delenv("DISPLAY", raising=False)
    monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
    assert notification._desktop_available() is False

    monkeypatch.setenv("DISPLAY", ":0")
    assert notification._desktop_available() is True


def test_desktop_available_darwin_always_ok(monkeypatch):
    """macOS has Notification Center; a tty is sufficient."""
    monkeypatch.setattr(notification.sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr(notification.sys, "platform", "darwin")
    monkeypatch.delenv("DISPLAY", raising=False)
    assert notification._desktop_available() is True
