# -*- coding: utf-8 -*-

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

        session = str(self.args.name[0])

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
    def complete(hosts, config, log, text, line, begidx, endidx):

        loc = RefhostsFactory(config, log).get_locations()

        locations = [[str(x) for x in loc]]

        return complete_choices(locations, line, text)
