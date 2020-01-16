import logging

from mtui import messages
from mtui.commands import Command
from mtui.connector.openqa import AutoOpenQA, KernelOpenQA
from mtui.refhost import RefhostsFactory
from mtui.utils import complete_choices, requires_update

logger = logging.getLogger("mtui.commands.simplesets")


class SessionName(Command):
    """
    Set optional mtui session name as part of the prompt string.
    This should help finding the corrent mtui session if multiple
    sessions are active.
    """

    command = "set_session_name"

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "name",
            action="store",
            type=str,
            nargs="?",
            default="",
            help="name of session",
        )

        return parser

    def __call__(self):

        session = self.args.name if self.args.name else self.metadata.id

        self.prompt.session = session

        self.prompt.set_prompt(session)


class SetLocation(Command):
    """
    Change current reference host location to another site.
    """

    command = "set_location"

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "site", action="store", type=str, nargs=1, help="location name"
        )

        return parser

    def __call__(self):

        old = self.config.location
        new = str(self.args.site[0])
        self.config.location = new
        logger.info(messages.LocationChangedMessage(old, new))

    @staticmethod
    def complete(state, text, line, begidx, endidx):

        loc = RefhostsFactory(state["config"]).get_locations()

        locations = [[str(x) for x in loc]]

        return complete_choices(locations, line, text)


class SetLogLevel(Command):
    """
       Changes the current MTUI loglevel "info" or "warning"  or "debug".

       To enable debug messages, one can set the loglevel to "debug".
       This could be handy for longer running commands as
       the output is shown in realtime.
       The "warning" loglevel prints just basic error or warning conditions.
       Therefore it's not recommended to use the "warning" loglevel.
    """

    command = "set_log_level"

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "level",
            action="store",
            type=str,
            nargs=1,
            choices=["info", "error", "warning", "debug"],
            help="log level for mtui - info, warning or debug",
        )

        return parser

    def __call__(self):
        levels = {
            "error": logging.ERROR,
            "warning": logging.WARNING,
            "info": logging.INFO,
            "debug": logging.DEBUG,
        }
        new = self.args.level[0]

        self.prompt.log.setLevel(level=levels[new])

        logger.info("Log level is set to {}".format(new))

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices(
            [("warning",), ("info",), ("debug",), ("error",)], line, text
        )


class SetTimeout(Command):
    """
    Changes the current execution timeout for a target host.
    When the timeout limit was hit the user is asked to wait
    for the current command to return or to proceed with the
    next one.
    The timeout value is set in seconds.
    To disable the timeout set it to "0".
    """

    command = "set_timeout"

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "timeout",
            action="store",
            type=int,
            nargs=1,
            help='Timeout in sec, "0" disables it',
        )

        cls._add_hosts_arg(parser)

        return parser

    def __call__(self):

        value = self.args.timeout[0]

        targets = self.parse_hosts()

        for target in targets:
            targets[target].set_timeout(value)
            logger.info("Timeout on {} is set to {}".format(target, value))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class SetWorkflow(Command):
    """Sets workflow and reload data from openQA\n
    'auto' workflow will be automaticaly set to manual if openQA install tests
    missiong or have failed state"""

    command = "set_workflow"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "workflow", choices=["auto", "manual", "kernel"], help="desired workflow"
        )
        return parser

    @requires_update
    def __call__(self):
        state = self.args.workflow

        if state == "kernel":
            if self.config.kernel:
                logger.info(f"Desired workflow {state} is same as current")
                self.metadata.openqa["auto"].run()
                for oq in self.metada.openqa["kernel"]:
                    oq.run()
                return
            else:
                logger.info(f"Setting workflow to '{state}'")
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
                logger.info(f"Desired workflow {state} is same as current")
                self.metadata.openqa["auto"].run()
                return
            else:
                logger.info(f"Setting workflow to '{state}'")
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
                logger.info(f"Desired workflow {state} is same as current")
                self.metadata.openqa["auto"].run()
                return
            else:
                logger.info(f"Setting workflow to '{state}'")
                self.config.auto = False
                self.config.kernel = False
                self.metadata.openqa["auto"].run()
                self.metadata.openqa["kernel"] = []
                return

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices([("auto",), ("manual",), ("kernel",)], line, text)
