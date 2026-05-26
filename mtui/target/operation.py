"""Template-method consolidation of the install/uninstall flow on a HostsGroup.

This module exists to keep the shared
``lock → run → check → reboot → unlock`` skeleton in one place rather than
duplicated across :meth:`HostsGroup.perform_install` and
:meth:`HostsGroup.perform_uninstall`. New ``install-shaped`` flows plug in
by subclassing :class:`Operation` and providing the two ``get_*`` hooks.

The wider :meth:`HostsGroup.perform_prepare`, :meth:`perform_downgrade`,
and :meth:`perform_update` flows are intentionally **not** routed through
this module: they have per-package loops, ``set_repo`` fanouts, and (in
the case of update) a nested two-phase try/finally for guaranteed repo
cleanup. Forcing them through a shared template would require enough
optional hooks that the result would be harder to read than the
originals.
"""

from abc import ABC, abstractmethod
from collections.abc import Callable
from logging import getLogger
from typing import TYPE_CHECKING, Any, ClassVar, final

from ..messages import MissingInstallerError, MissingUninstallerError
from . import Target

if TYPE_CHECKING:
    from .hostgroup import HostsGroup

logger = getLogger("mtui.target.operation")


class Operation(ABC):
    """Template method consolidating the install/uninstall flow on a HostsGroup.

    Subclasses provide the "doer" (installer/uninstaller dict) and the
    paired check callable for a target. The base class drives the shared
    skeleton:

        1. collect commands + reboot dicts (early-return on ``missing_error``)
        2. ``group.update_lock()``
        3. in ``try``: ``group.run(commands)`` → per-host check → ``group._reboot``
        4. ``finally``: ``group.unlock()``

    Behaviour preserved byte-for-byte from the previous open-coded
    ``HostsGroup.perform_install`` / ``perform_uninstall`` implementations.
    """

    role: ClassVar[str]
    missing_error: ClassVar[type[Exception]]

    def __init__(self, group: "HostsGroup", packages: list[str]) -> None:
        """Initialise the operation.

        Args:
            group: The :class:`HostsGroup` to operate on.
            packages: Package names to pass through to the doer's command
                template via ``packages=" ".join(packages)``.

        """
        self.group = group
        self.packages = packages

    @abstractmethod
    def get_doer(self, target: Target) -> dict[str, Any]:
        """Return the doer dict (e.g. installer/uninstaller) for ``target``.

        Subclass responsibility. The returned mapping must contain at
        least ``"command"`` and ``"reboot"`` ``string.Template``-like
        entries exposing ``substitute(...)``.
        """

    @abstractmethod
    def get_check(self, target: Target) -> Callable[..., None]:
        """Return the post-run check callable for ``target``.

        Subclass responsibility. Invoked as
        ``check(hostname, lastout, lastin, lasterr, lastexit)``.
        """

    def collect(self) -> tuple[dict[str, str], dict[str, str]]:
        """Build the per-host ``commands`` and (transactional) ``reboot`` dicts."""
        commands = {
            t.hostname: self.get_doer(t)["command"].substitute(
                packages=" ".join(self.packages)
            )
            for t in self.group.data.values()
        }
        reboot = {
            t.hostname: self.get_doer(t)["reboot"].substitute()
            for t in self.group.data.values()
            if t.transactional
        }
        return commands, reboot

    def run(self) -> None:
        """Execute the full lock → run → check → reboot → unlock skeleton."""
        try:
            commands, reboot = self.collect()
        except self.missing_error as e:
            logger.error("%s", e)
            return

        self.group.update_lock()
        try:
            self.group.run(commands)
            for t in self.group.data.values():
                self.get_check(t)(
                    t.hostname, t.lastout(), t.lastin(), t.lasterr(), t.lastexit()
                )
            self.group._reboot(reboot)  # noqa: SLF001
        finally:
            self.group.unlock()


@final
class InstallOperation(Operation):
    """Install ``packages`` on every target in the group."""

    role: ClassVar[str] = "installer"
    missing_error: ClassVar[type[Exception]] = MissingInstallerError

    def get_doer(self, target: Target) -> dict[str, Any]:
        return target.get_installer()

    def get_check(self, target: Target) -> Callable[..., None]:
        return target.get_installer_check()


@final
class UninstallOperation(Operation):
    """Uninstall ``packages`` from every target in the group."""

    role: ClassVar[str] = "uninstaller"
    missing_error: ClassVar[type[Exception]] = MissingUninstallerError

    def get_doer(self, target: Target) -> dict[str, Any]:
        return target.get_uninstaller()

    def get_check(self, target: Target) -> Callable[..., None]:
        return target.get_uninstaller_check()
