import subprocess
from logging import getLogger
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices, edit_text, requires_update

logger = getLogger("mtui.command.testopiaedit")


class TestopiaEdit(Command):

    """
    Edit already existing Testopia package testcase.
    An editor is spawned to process a testcase template file.
    """

    command = "testopia_edit"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("testcase_id", help="Test case id")
        return parser

    @requires_update
    def __call__(self):
        self.prompt.ensure_testopia_loaded()
        keywords = [
            "summary",
            "automated",
            "status",
            "requirement",
            "setup",
            "breakdown",
            "action",
            "effect",
        ]
        candidates = []

        # Finds the test case with the inputs provided by the user
        try:
            candidates = [str(int(self.args.testcase_id))]
        except ValueError:
            candidates = [
                k
                for k, v in list(self.prompt.testopia.testcases.items())
                if v["summary"].replace("_", " ")
                in self.args.testcase_id.replace("_", " ")
            ]

        if not candidates:
            logger.warning("No testcase found")
            return
        elif len(candidates) > 1:
            logger.warning(
                "Possible candidates found: {!s}. Please be more specific".format(
                    candidates
                )
            )
            return

        # We found the testcase. Let's print it to the user
        testcase = self.prompt.testopia.get_testcase(candidates[0])
        if not testcase:
            return

        template = []
        for field in keywords:
            template.append("{!s}: {!s}".format(field, testcase[field]))

        try:
            edited_text = edit_text("\n".join(template))
        except subprocess.CalledProcessError as e:
            logger.error("editor failed: {!s}".format(e))
            logger.debug(format_exc())
            return

        edited_text = edited_text.strip()
        if edited_text == "\n".join(template):
            logger.warning("testcase was not modified. not updating.")
            return

        template_text = edited_text.replace("\n", "|br|")

        for field in keywords:
            template_text = template_text.replace(
                "|br|{!s}".format(field), "\n{!s}".format(field)
            )

        lines = template_text.split("\n")
        for line in lines:
            key, _, value = line.partition(":")
            testcase[key] = value.strip()

        try:
            self.prompt.testopia.modify_testcase(candidates[0], testcase)
        except Exception:
            logger.error("failed to modify testcase {!s}".format(candidates[0]))
        else:
            logger.info(
                "testcase saved: {!s}/tr_show_case.cgi?case_id={!s}".format(
                    self.config.bugzilla_url, candidates[0]
                )
            )

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        testcases = [
            (i["summary"].replace(" ", "_"),)
            for i in list(state["testopia"].testcases.values())
        ]

        return complete_choices(testcases, line, text)
