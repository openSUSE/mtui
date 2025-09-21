"""This package contains the target-related classes for mtui.

Each module in this package defines a class that is related to
target hosts, such as the `Target` class itself, or classes for
managing locks on target hosts.
"""

from .locks import LockedTargets, RemoteLock, TargetLock, TargetLockedError
from .target import Target


__all__ = ["Target", "TargetLock", "RemoteLock", "TargetLockedError", "LockedTargets"]
