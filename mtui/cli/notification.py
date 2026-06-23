"""A simple desktop notification system backed by :mod:`notifypy`.

Desktop notifications are an opt-in feature (the ``notify`` extra pulls in
`notify-py <https://pypi.org/project/notify-py/>`_). When the dependency is
absent, or the process is not attached to an interactive desktop session,
:func:`display` degrades to a quiet no-op so headless, piped, cron, and MCP
runs never attempt to pop a toast. ``notify-py`` talks to the freedesktop
DBus notification service via pure-Python ``jeepney`` on Linux, so no system
GTK/libnotify Python bindings are required.
"""

import os
import sys
from logging import getLogger
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from notifypy import Notify  # ty: ignore[unresolved-import]

logger = getLogger("mtui.notifications")

#: Resolved ``notifypy.Notify`` class, or ``None`` when unavailable.
_notify_cls: "type[Notify] | None" = None
#: Whether an import has already been attempted (so a missing extra costs a
#: single debug-logged attempt rather than retrying on every notification).
_resolved = False


def _resolve() -> "type[Notify] | None":
    """Returns the ``notifypy.Notify`` class, or ``None`` when unavailable."""
    global _notify_cls, _resolved
    if not _resolved:
        _resolved = True
        try:
            from notifypy import Notify  # ty: ignore[unresolved-import]
        except ImportError:
            logger.debug("notify-py not installed. notification disabled.")
        else:
            _notify_cls = Notify

    return _notify_cls


def _desktop_available() -> bool:
    """Reports whether a desktop notification can plausibly be shown.

    Notifications are a REPL-only courtesy. A toast only makes sense when a
    user is sitting at an interactive terminal with a graphical session, so
    this guards against piped/cron/CI/MCP runs that would otherwise attempt
    (and fail at) a desktop pop-up.
    """
    if not sys.stdin.isatty():
        return False

    if sys.platform == "darwin":
        return True

    # Linux/BSD: a freedesktop notification needs a graphical session.
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))


def display(
    summary: str | None = None,
    text: str | None = None,
    icon: str | None = None,
) -> None:
    """Displays a desktop notification.

    Args:
        summary: The summary (title) text of the notification.
        text: The body text of the notification.
        icon: Path to an icon image to display, or ``None`` for the default.

    """
    if not _desktop_available():
        return

    notify_cls = _resolve()
    if notify_cls is None:
        return

    logger.debug('displaying notify message "%s"', text)
    try:
        notification = notify_cls(default_application_name="mtui")
        if summary is not None:
            notification.title = summary
        if text is not None:
            notification.message = text
        if icon:
            notification.icon = icon
        notification.send(block=False)
    except Exception:
        logger.debug("failed to display notification")
