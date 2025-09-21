"""A collection of simple "set" commands."""

import logging

from mtui import messages
from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.connector.openqa import AutoOpenQA, KernelOpenQA
from mtui.refhost import RefhostsFactory
from mtui.utils import complete_choices, requires_update

logger = logging.getLogger("mtui.commands.simplesets")


class SessionName(Command):
    """Sets the optional mtui session name as part of the prompt string.

    This can help to identify the correct mtui session if multiple
    sessions are active.
    """

    command = "set_session_name"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "name",
            action="store",
            type=str,
            nargs="?",
            default="",
            help="name of session",
        )

    def __call__(self) -> None:
        """Executes the `set_session_name` command."""
        session = self.args.name if self.args.name else self.metadata.id
        self.prompt.session = session
        self.prompt.set_prompt(session)


class SetLocation(Command):
    """Changes the current reference host location to another site."""

    command = "set_location"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "site", action="store", type=str, nargs=1, help="location name"
        )

    def __call__(self) -> None:
        """Executes the `set_location` command."""
        old: str = self.config.location
        new: str = self.args.site[0]
        self.config.location = new
        logger.info(messages.LocationChangedMessage(old, new))

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        loc = RefhostsFactory(state["config"]).get_locations()
        locations = [[x for x in loc]]

        return complete_choices(locations, line, text)


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

        logger.info("Log level is set to {}".format(new))

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
            if self.config.kernel:
                logger.info("Desired workflow %s is same as current", state)
                self.metadata.openqa["auto"].run()
                for oq in self.metadata.openqa["kernel"]:
                    oq.run()
                return
            else:
                logger.info("Setting workflow to '%s'", state)
                self.config.auto = False
                self.config.kernel = True
                self.metadata.openqa["auto"] = AutoOpenQA(
                    self.config,
                    self.config.openqa_instance,
                    self.metadata.smelt,
                    self.metadata.id,
                ).run()
                self.metadata.openqa["kernel"] = []
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
                return
        elif state == "auto":
            if self.config.auto:
                logger.info("Desired workflow %s is same as current", state)
                self.metadata.openqa["auto"].run()
                return
            else:
                logger.info("Setting workflow to '%s'", state)
                self.config.auto = True
                self.config.kernel = False
                self.metadata.openqa["auto"] = AutoOpenQA(
                    self.config,
                    self.config.openqa_instance,
                    self.metadata.smelt,
                    self.metadata.id,
                ).run()
                self.metadata.openqa["kernel"] = []
                if self.metadata.openqa["auto"].results is None:
                    logger.warning("No install jobs or install jobs failed")
                    logger.info("Switch mode to manual")
                    self.config.auto = False
                return
        else:
            if not self.config.auto and not self.config.kernel:
                logger.info("Desired workflow %s is same as current", state)
                self.metadata.openqa["auto"].run()
                return
            else:
                logger.info("Setting workflow to '%s'", state)
                self.config.auto = False
                self.config.kernel = False
                self.metadata.openqa["auto"].run()
                self.metadata.openqa["kernel"] = []
                return

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("auto",), ("manual",), ("kernel",)], line, text)
