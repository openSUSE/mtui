"""Command-timeout error and host-key policy mapping.

These helpers live alongside :class:`~mtui.hosts.connection.connection.Connection`
but have no dependency on it, so they sit in their own module to keep
``connection.py`` focused on the SSH/SFTP wrapper proper.
"""

from logging import getLogger

import paramiko

logger = getLogger("mtui.connection")


class CommandTimeoutError(Exception):
    """Exception raised when a remote command times out."""

    def __init__(self, command=None) -> None:
        """Initializes the exception.

        Args:
            command: The command that timed out.

        """
        self.command = command

    def __str__(self) -> str:
        """Returns the timed out remote command as a string."""
        return repr(self.command)


class NonInteractiveAuthRequired(paramiko.AuthenticationException):
    """Raised when key auth fails but no interactive password prompt is possible.

    SSH public-key authentication failed and the only remaining fallback
    is to ask the user for the root password. In a non-interactive
    session (e.g. ``mtui-mcp``, which has no TTY and whose stdin is the
    JSON-RPC pipe) that prompt cannot be shown and would block forever,
    so we raise this instead.

    Subclasses :class:`paramiko.AuthenticationException` so existing
    ``except (paramiko.)AuthenticationException`` / ``except Exception``
    handlers in the connect path keep catching it -- the host is simply
    reported as unreachable rather than hanging the process.
    """


_HOST_KEY_POLICIES: dict[str, type[paramiko.MissingHostKeyPolicy]] = {
    "auto_add": paramiko.AutoAddPolicy,
    "warn": paramiko.WarningPolicy,
    "reject": paramiko.RejectPolicy,
}


def policy_from_config(name: str) -> paramiko.MissingHostKeyPolicy:
    """Map an ``ssh_strict_host_key_checking`` config value to a paramiko policy.

    Unknown values fall back to ``AutoAddPolicy`` (preserving the legacy
    behaviour) and emit a warning so misconfigurations are visible.
    """
    cls = _HOST_KEY_POLICIES.get(name)
    if cls is None:
        logger.warning(
            "unknown ssh_strict_host_key_checking=%r; falling back to auto_add",
            name,
        )
        cls = paramiko.AutoAddPolicy
    return cls()
