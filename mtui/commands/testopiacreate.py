# -*- coding: utf-8 -*-

import subprocess
import re
from traceback import format_exc
from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update
from mtui.utils import edit_text
from argparse import REMAINDER


class TestopiaCreate(Command):
    """
    Create new Testopia package testcase.
    An editor is spawned to process a testcase template file.
    """
    command = 'testopia_create'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "package",
            help='package to create testcases for')

        parser.add_argument(
            "summary",
            nargs=REMAINDER,
            help='summary of the testcase')

        return parser

    @requires_update
    def run(self):
        self.prompt.ensure_testopia_loaded()
        keywords = [
            'summary',
            'package',
            'automated',
            'status',
            'requirement',
            'setup',
            'breakdown',
            'action',
            'effect']
        testcase = dict.fromkeys(keywords, '')

        # Build the text to show to the user for presentation
        text = ''
        for keyword in keywords:
            text += keyword+':'
            if keyword == 'summary':
                text += ' {0}'.format(' '.join(self.args.summary))
            elif keyword == 'package':
                text += ' {0}'.format(self.args.package)
            elif keyword == 'automated':
                text += ' no'
            elif keyword == 'status':
                text += ' proposed'

            text += '\n'

        text = text.strip()
        try:
            edited = edit_text(text)
        except subprocess.CalledProcessError as e:
            self.log.error("editor failed: {!s}".format(e))
            self.log.debug(format_exc())
            return

        if edited == text:
            self.log.warning('testcase was not modified. not uploading.')
            return

        # Parse what the user saved. We need to fill the testcase
        keyword_regexp = re.compile('('+'|'.join(keywords)+'):', re.IGNORECASE)

        current_keyword = ''
        for line in edited.split('\n'):
            match = keyword_regexp.match(line)
            if match:
                (header, _, content) = line.partition(':')
                current_keyword = header
                testcase[current_keyword] += content.strip()
            else:
                testcase[current_keyword] += '|br|'+line

        # the testcase needs an extra 'tags' key
        testcase['tags'] = 'packagename_{0},testcase_{0}'.format(testcase[
                                                                 'package'])
        del testcase['package']

        # Upload the test case
        try:
            case_id = self.prompt.testopia.create_testcase(testcase)
        except Exception:
            self.log.error('failed to create testcase')
        else:
            self.log.info(
                'created testcase {!s}/tr_show_case.cgi?case_id={!s}'.format(
                    self.config.bugzilla_url, case_id))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        packages = [
                    tuple(
                        package
                        for package in state['metadata'].get_package_list())]
        return complete_choices(packages, line, text)
