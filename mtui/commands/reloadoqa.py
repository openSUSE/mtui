"""The `reload_openqa` command."""

from logging import getLogger

from ..connector.openqa import KernelOpenQA
from ..connector.qem_dashboard import DashboardAutoOpenQA
from ..utils import requires_update
from . import Command

logger = getLogger("mtui.commands.reloadopenqa")


class ReloadOpenQA(Command):
    """Reloads information from openQA instances."""

    command = "reload_openqa"

    @requires_update
    def __call__(self) -> None:
        """Executes the `reload_openqa` command."""
        if self.config.kernel:
            if self.metadata.openqa.kernel == []:
                logger.info("Getting data from kernel openQA")
                self.metadata.openqa.kernel.append(
                    KernelOpenQA(
                        self.config,
                        self.config.openqa_instance,
                        self.metadata.incident,
                        self.metadata.rrid,
                    ).run()
                )
                self.metadata.openqa.kernel.append(
                    KernelOpenQA(
                        self.config,
                        self.config.openqa_instance_baremetal,
                        self.metadata.incident,
                        self.metadata.rrid,
                    ).run()
                )
            else:
                logger.info("Refreshing data from kernel openQA")
                for oqa in self.metadata.openqa.kernel:
                    oqa.run()

        if self.metadata.openqa.auto is None:
            logger.info("Getting data from QEM Dashboard")
            self.metadata.openqa.auto = DashboardAutoOpenQA(
                self.config,
                self.config.openqa_instance,
                self.metadata.incident,
                self.metadata.rrid,
            ).run()
        else:
            logger.info("Refreshing data from QEM Dashboard")
            self.metadata.openqa.auto.run()
