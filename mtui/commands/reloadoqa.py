from logging import getLogger

from ..connector.openqa import AutoOpenQA, KernelOpenQA
from ..utils import requires_update
from . import Command

logger = getLogger("mtui.commands.reloadopenqa")


class ReloadOpenQA(Command):
    """
    Reload's informations from openQA instances
    """

    command = "reload_openqa"

    @requires_update
    def __call__(self):
        if self.config.kernel:
            if self.metadata.openqa["kernel"] == []:
                logger.info("Getting data from kernel openQA")
                self.metadata.openqa["kernel"].append(
                    KernelOpenQA(
                        self.config,
                        self.config.openqa_instance,
                        self.metadata.smelt,
                        self.metadata.id,
                    ).run()
                )
                self.metadata.openqa["kernel"].append(
                    KernelOpenQA(
                        self.config,
                        self.config.openqa_instance_baremetal,
                        self.metadata.smelt,
                        self.metadata.id,
                    ).run()
                )
            else:
                logger.info("Refreshing data from kernel openQA")
                for oqa in self.metadata.openqa["kernel"]:
                    oqa.run()

        if self.metadata.openqa["auto"] is None:
            logger.info("Getting data from openQA")
            self.metadata.openqa["auto"] = AutoOpenQA(
                self.config,
                self.config.openqa_instance,
                self.metadata.smelt,
                self.metadata.id,
            ).run()
        else:
            logger.info("Refreshing data from openQA")
            self.metadata.openqa["auto"].run()
