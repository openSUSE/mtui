"""SSH/SFTP connection (paramiko wrapper) and timeout helpers.

Re-exports the public surface so callers can write
``from mtui.hosts.connection import Connection`` without learning the
internal leaf-module split.
"""

from .connection import Connection
from .timeout import CommandTimeoutError, policy_from_config

__all__ = [
    "CommandTimeoutError",
    "Connection",
    "policy_from_config",
]
