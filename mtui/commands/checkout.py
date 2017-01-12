# -*- coding: utf-8 -*-

from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import requires_update


class Checkout(Command):
    """
    Update template files from the SVN.
    """
    command = 'checkout'

    @requires_update
    def run(self):
        try:
            check_call('svn up'.split(), cwd=self.metadata.report_wd())
        except Exception:
            self.log.error('updating template failed')
            self.log.debug(format_exc())
