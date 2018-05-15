
import os

from os.path import join

from mtui.template.testreport import TestReport


class NullTestReport(TestReport):
    _type = "No"

    def __init__(self, *a, **kw):
        super(NullTestReport, self).__init__(*a, **kw)
        self.id = None
        self.path = join(os.getcwd(), "None")

    def __bool__(tr):
        return False

    def target_wd(self, *paths):
        return join(self.config.target_tempdir, *paths)

    def _get_updater_id(tr):
        return None

    def _parser(tr):
        return None

    def _update_repos_parser(tr):
        return {}
