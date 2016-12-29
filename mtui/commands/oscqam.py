# -*- coding: utf-8 -*-

from argparse import REMAINDER
from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import requires_update
from mtui.utils import complete_choices

osc_api = {'SUSE': 'https://api.suse.de',
           'openSUSE': 'https://api.opensuse.org'}


class OSCAssign(Command):
    """
    Wrapper on 'osc qam assign' command, assings you current update.
    Can be specified groups for assigment
    """
    command = 'assign'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-g',
            '--group',
            nargs='?',
            action='append',
            help="Group wanted to assign")
        return parser

    @requires_update
    def run(self):
        apiid, _, _, reviewid = str(self.metadata.id).split(':')
        self.log.info("Assign request: {}".format(reviewid))
        cmd = 'osc -A {} qam assign'.format(osc_api[apiid])
        group = ' '

        if self.args.group:
            for i in self.args.group:
                group += ''.join('-G ' + i)

        cmd += group + ' ' + reviewid
        self.log.debug(cmd)

        try:
            check_call(cmd.split())
        except Exception as e:
            self.log.info('Assign failed: {!s}'.format(e))
            self.log.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([('-g', '--group'), ], line, text)


class OSCApprove(Command):
    """
    Wrapper around 'osc qam approve' commad. It's possible to
    specify more groups to approve
    """
    command = 'approve'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-g',
            '--group',
            nargs='?',
            action='append',
            help="Group wanted by user to approve")
        return parser

    @requires_update
    def run(self):
        apiid, _, _, reviewid = str(self.metadata.id).split(':')
        self.log.info("Approve request: {}".format(reviewid))
        cmd = 'osc -A {} qam approve'.format(osc_api[apiid])
        group = ' '

        if self.args.group:
            for i in self.args.group:
                group += ''.join('-G ' + i)

        cmd += group + ' ' + reviewid
        self.log.debug(cmd)

        try:
            check_call(cmd.split())
        except Exception as e:
            self.log.info('Approve failed: {!s}'.format(e))
            self.log.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([('-g', '--group'), ], line, text)


class OSCReject(Command):
    """
    Wrapper around 'osc qam reject', '-r'  option is required.
    """
    command = 'reject'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-g',
            '--group',
            nargs='?',
            action='append',
            help="Group wanted by user to approve")
        parser.add_argument(
            '-r',
            '--reason',
            required=True,
            choices=[
                'admin',
                'retracted',
                'build_problem',
                'not_fixed',
                'regression',
                'false_reject',
                'tracking_issue'],
         help='Reason to reject update, required')
        parser.add_argument(
            '-m', '--msg', nargs=REMAINDER,
            help="Message to use for rejection-comment." +
            "Always as last part of command please")
        return parser

    @requires_update
    def run(self):
        apiid, _, _, reviewid = str(self.metadata.id).split(':')
        self.log.info("Reject request: {}".format(reviewid))
        cmd = 'osc -A {} qam reject'.format(osc_api[apiid])
        group = ' '

        if self.args.group:
            for i in self.args.group:
                group += ''.join('-G ' + i)

        reason = '-R ' + self.args.reason

        message = ' '
        if self.args.msg:
            message = '-M '
            message += ' '.join(self.args.msg)

        cmd += group + ' ' + reason + ' ' + reviewid + ' ' + message
        self.log.debug(cmd)

        try:
            check_call(cmd.split())
        except Exception as e:
            self.log.info('Reject failed: {!s}'.format(e))
            self.log.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices(
            [('-g', '--group',),
             ('-r', '--reason'),
             ('-m', '--msg'),
             ('admin', 'retracted', 'build_problem', 'not_fixed', 'regression',
              'false_reject', 'tracking_issue')],
            line, text)
