# -*- coding: utf-8 -*-

import os

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices_filelist
from mtui.utils import requires_update
from mtui.utils import prompt_user
from mtui.utils import timestamp


class Export(Command):
    """
    Exports the gathered update data to template file. This includes
    the pre/post package versions and the update log. An output file could
    be specified, if none is specified, the output is written to the
    current testing template.
    To export a specific updatelog, provide the hostname as parameter.
    """
    command = 'export'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-f', '--force', action='store_true',
            help='force overwrite existing template')
        parser.add_argument(
            '-n',
            '--hostname',
            default=None,
            help='host update log to export')
        parser.add_argument(
            'filename',
            nargs='?',
            help='output template file name')
        return parser

    @requires_update
    def run(self):
        filename = self.args.filename if self.args.filename else self.metadata.path

        try:
            template = self.metadata.generate_templatefile(self.args.hostname)
        except Exception as e:
            self.log.error('Failed to export XML')
            self.log.error(e)
            self.log.debug(format_exc())
            return

        if os.path.exists(filename) and not self.args.force:
            self.log.warning('file {!s} exists.'.format(filename))
            if not prompt_user(
                    'Should I overwrite {!s} (y/N) '.format(filename),
                    ['y', 'Y', 'yes', 'Yes', 'YES'],
                    self.prompt.interactive):
                filename += '.' + timestamp()

        self.log.info('exporting XML to {!s}'.format(filename))

        try:
            with open(filename, 'w') as f:
                f.write('\n'.join(line.rstrip().encode('utf-8')
                                  for line in template))
        except IOError as e:
            self.println('Failed to write {}: {}'.format(filename, e.strerror))
            return

        self.println('wrote template to {}'.format(filename))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        clist = [('-f', '--force'), ('-n', '--hostname')]
        return complete_choices_filelist(
            clist, line, text, state['hosts'].names())
