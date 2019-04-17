# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update


class TestopiaShow(Command):
    """
    Show Testopia testcase
    """

    command = "testopia_show"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-t",
            "--testcase",
            action="append",
            default=[],
            type=str,
            required=True,
            help="testcase to show",
        )

        return parser

    @requires_update
    def __call__(self):
        self.prompt.ensure_testopia_loaded()

        url = self.config.bugzilla_url
        cases = []
        for case in self.args.testcase:
            case = case.replace("_", " ")
            try:
                cases.append(str(int(case)))
            except ValueError:
                cases += [
                    k
                    for k, v in list(self.prompt.testopia.testcases.items())
                    if v["summary"].replace("_", " ") in case
                ]

        for case_id in cases:
            testcase = self.prompt.testopia.get_testcase(case_id)

            if not testcase:
                continue

            self.display.testopia_show(
                url,
                case_id,
                testcase["summary"],
                testcase["status"],
                testcase["automated"],
                testcase["requirement"],
                testcase["setup"],
                testcase["action"],
                testcase["breakdown"],
                testcase["effect"],
            )

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        testcases = [
            (i["summary"].replace(" ", "_"),)
            for i in list(state["testopia"].testcases.values())
        ]
        testcases += [("-t", "--testcase")]
        return complete_choices(testcases, line, text)
