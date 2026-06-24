"""A collection of simple "set" commands."""

import logging

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..data_sources.openqa import KernelOpenQA
from ..data_sources.qem_dashboard import DashboardAutoOpenQA
from ..support.misc import requires_update
from ..types import Workflow
from . import Command

logger = logging.getLogger("mtui.commands.simplesets")


class SetLogLevel(Command):
    """Changes the current MTUI log level.

    To enable debug messages, set the log level to "debug". This can
    be useful for longer running commands, as the output is shown in
    realtime. The "warning" log level only prints basic error or
    warning conditions and is therefore not recommended.
    """

    command = "set_log_level"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "level",
            action="store",
            type=str,
            nargs=1,
            choices=["info", "error", "warning", "debug"],
            help="log level for mtui - info, warning or debug",
        )

    def __call__(self) -> None:
        """Executes the `set_log_level` command."""
        levels: dict[str, int] = {
            "error": logging.ERROR,
            "warning": logging.WARNING,
            "info": logging.INFO,
            "debug": logging.DEBUG,
        }
        new: str = self.args.level[0]

        self.prompt.log.setLevel(level=levels[new])

        logger.info("Log level is set to %s", new)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("warning",), ("info",), ("debug",), ("error",)], line, text
        )


class SetTimeout(Command):
    """Changes the current execution timeout for a target host.

    When the timeout limit is hit, the user is asked to wait for the
    current command to return or to proceed with the next one. The
    timeout value is set in seconds. To disable the timeout, set it
    to "0".
    """

    command = "set_timeout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "timeout",
            action="store",
            type=int,
            nargs=1,
            help='Timeout in sec, "0" disables it',
        )

        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `set_timeout` command."""
        value: int = self.args.timeout[0]
        targets = self.parse_hosts()

        for target in targets:
            targets[target].set_timeout(value)
            logger.info("Timeout on %s is set to %d", target, value)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class SetWorkflow(Command):
    """Sets the workflow and reloads data from openQA.

    The 'auto' workflow will be automatically set to 'manual' if openQA
    install tests are missing or have a failed state.
    """

    command = "set_workflow"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "workflow", choices=["auto", "manual", "kernel"], help="desired workflow"
        )

    @requires_update
    def __call__(self) -> None:
        """Executes the `set_workflow` command."""
        state: str = self.args.workflow

        if state == "kernel":
            if self.metadata.workflow is Workflow.KERNEL:
                logger.info("Desired workflow %s is same as current", state)
                self.metadata.openqa.auto.run()
                for oq in self.metadata.openqa.kernel:
                    oq.run()
                return
            logger.info("Setting workflow to '%s'", state)
            self.metadata.workflow = Workflow.KERNEL
            self.metadata.openqa.auto = DashboardAutoOpenQA(
                self.config,
                self.config.openqa_instance,
                self.metadata.incident,
                self.metadata.rrid,
            ).run()
            self.metadata.openqa.kernel = []
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
            return
        if state == "auto":
            if self.metadata.workflow is Workflow.AUTO:
                logger.info("Desired workflow %s is same as current", state)
                self.metadata.openqa.auto.run()
                return
            logger.info("Setting workflow to '%s'", state)
            self.metadata.workflow = Workflow.AUTO
            self.metadata.openqa.auto = DashboardAutoOpenQA(
                self.config,
                self.config.openqa_instance,
                self.metadata.incident,
                self.metadata.rrid,
            ).run()
            self.metadata.openqa.kernel = []
            if self.metadata.openqa.auto.results is None:
                logger.warning("No install jobs or install jobs failed")
                logger.info("Switch mode to manual")
                self.metadata.workflow = Workflow.MANUAL
            return
        if self.metadata.workflow is Workflow.MANUAL:
            logger.info("Desired workflow %s is same as current", state)
            self.metadata.openqa.auto.run()
            return
        logger.info("Setting workflow to '%s'", state)
        self.metadata.workflow = Workflow.MANUAL
        self.metadata.openqa.auto.run()
        self.metadata.openqa.kernel = []
        return

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("auto",), ("manual",), ("kernel",)], line, text)
