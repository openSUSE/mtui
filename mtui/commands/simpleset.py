# -*- coding: utf-8 -*-

import logging

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui import messages
from mtui.refhost import RefhostsFactory


class SessionName(Command):
    """
    Set optional mtui session name as part of the prompt string.
    This should help finding the corrent mtui session if multiple
    sessions are active.
    """

    command = 'set_session_name'

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "name",
            action='store',
            type=str,
            nargs='?',
            default='',
            help="name of session")

        return parser

    def run(self):

        session = self.args.name if self.args.name else self.metadata.id

        self.prompt.session = session

        self.prompt.set_prompt(session)


class SetLocation(Command):
    """
    Change current reference host location to another site.
    """
    command = 'set_location'

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "site",
            action='store',
            type=str,
            nargs=1,
            help="location name")

        return parser

    def run(self):

        old = self.config.location
        new = str(self.args.site[0])
        self.config.location = new
        self.log.info(messages.LocationChangedMessage(old, new))

    @staticmethod
    def complete(state, text, line, begidx, endidx):

        loc = RefhostsFactory(state['config'], state['log']).get_locations()

        locations = [[str(x) for x in loc]]

        return complete_choices(locations, line, text)


class SetLogLevel(Command):
    """
       Changes the current MTUI loglevel "info" or "warning"
       or "debug". To enable debug messages, one can set the loglevel
       to "debug". This could be handy for longer running commands as
       the output is shown in realtime. The "warning" loglevel prints
       just basic error or warning conditions. Therefore it's not
       recommended to use the "warning" loglevel.
    """
    command = 'set_log_level'

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "level",
            action="store",
            type=str,
            nargs=1,
            choices=['info', 'error', 'warning', 'debug'],
            help="log level for mtui - info, warning or debug")

        return parser

    def run(self):
        levels = {
            'error': logging.ERROR,
            'warning': logging.WARNING,
            'info': logging.INFO,
            'debug': logging.DEBUG}
        new = self.args.level[0]

        self.log.setLevel(level=levels[new])

        self.log.info('Log level is set to {}'.format(new))

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices(
            [('warning',), ('info',), ('debug',), ('error', ), ], line, text)


class SetTimeout(Command):
    """
    Changes the current execution timeout for a target host.
    When the timeout limit was hit the user is asked to wait
    for the current command to return or to proceed with the
    next one.
    The timeout value is set in seconds. To disable the
    timeout set it to "0".
    """

    command = 'set_timeout'

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "timeout",
            action='store',
            type=int,
            nargs=1,
            help='Timeout in sec, "0" disables it')

        cls._add_hosts_arg(parser)

        return parser

    def run(self):

        value = self.args.timeout[0]

        targets = self.parse_hosts()

        for target in targets:
            targets[target].set_timeout(value)
            self.log.info('Timeout on {} is set to {}'.format(target, value))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
