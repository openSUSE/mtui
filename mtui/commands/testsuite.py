import os
import re
import subprocess
from datetime import date
from logging import getLogger
from traceback import format_exc

from mtui import messages
from mtui.commands import Command
from mtui.utils import complete_choices, edit_text, requires_update

logger = getLogger("mtui.command.testsuite")


class TestSuiteList(Command):
    """
    List available testsuites on the target hosts.
    """

    command = "testsuite_list"

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def __call__(self):
        targets = self.parse_hosts()

        targets.report_testsuites(
            self.display.testsuite_list, self.config.target_testsuitedir
        )

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class TestSuiteRun(Command):
    """
    Runs ctcs2 testsuite and saves logs to /var/log/qa/RIDD on the
    target hosts. Results can be submitted with the testsuite_submit
    command.
    """

    command = "testsuite_run"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("testsuite", nargs=1, help="testsuite-run command")
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def __call__(self):
        targets = self.parse_hosts()
        cmd = self.args.testsuite[0]

        if not cmd.startswith("/"):
            cmd = self.config.target_testsuitedir / cmd.strip()

        cmd = "export TESTS_LOGDIR=/var/log/qa/{}; {}".format(
            self.metadata.id, str(cmd)
        )
        name = os.path.basename(cmd).replace("-run", "")

        try:
            targets.run(cmd)
        except KeyboardInterrupt:
            logger.info("testsuite run canceled")
            return

        for _, t in list(targets.items()):
            t.report_testsuite_results(self.display.testsuite_run, name)

        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class TestSuiteSubmit(Command):
    """
    Submits the ctcs2 testsuite results to qadb2.suse.de.
    The comment field is populated with some attributes like RRID or
    testsuite name, but can also be edited before the results get
    submitted.
    """

    command = "testsuite_submit"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("testsuite", nargs=1, help="testsuite-run command")
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def __call__(self):
        targets = self.parse_hosts()
        cmd = self.args.testsuite[0]
        name = os.path.basename(cmd).replace("-run", "")
        username = self.config.session_user

        comment = self.metadata.get_testsuite_comment(
            name, date.today().strftime("%d/%m/%y")
        )

        try:
            comment = edit_text(comment)
        except subprocess.CalledProcessError as e:
            logger.error("editor failed: {!s}".format(e))
            logger.debug(format_exc())
            return

        if len(comment) > 99:
            logger.warning(messages.QadbReportCommentLengthWarning())

        cmd = (
            "DISPLAY=dummydisplay:0 /usr/share/qa/tools/remote_qa_db_report.pl"
            + " -b -t patch:{0} -T {1} -f /var/log/qa/{0} -c '{2}'".format(
                self.metadata.id, username, comment
            )
        )

        try:
            for hostname, target in list(targets.items()):
                logger.info(
                    "Submiting results of {}-run from {}".format(name, hostname)
                )
                target.run(cmd)
        except KeyboardInterrupt:
            logger.info("Testsuite results submission canceled")
            return

        for hostname, target in list(targets.items()):
            if target.lastexit() != 0:
                logger.critical(
                    "submitting testsuite results failed on {!s}".format(hostname)
                )
                self.println("{}:~> {} [{}]".format(hostname, name, target.lastexit()))
                self.println(target.lastout())
                if target.lasterr():
                    self.println(target.lasterr())
            else:
                match = re.search(
                    "(http://.*/submission.php.submission_id=\d+)", target.lasterr()
                )
                if match:
                    logger.info(
                        "submission for {!s} ({!s}): {!s}".format(
                            hostname, target.system, match.group(1)
                        )
                    )
                else:
                    logger.critical(
                        'no submission found for {0!s}. please use "show_log -t {0!s}" to see what went wrong'.format(
                            hostname
                        )
                    )

        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
