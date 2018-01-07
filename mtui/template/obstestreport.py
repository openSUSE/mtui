from os.path import join
from mtui.template.testreport import TestReport
from mtui.parsemeta import OBSMetadataParser
from mtui.template.repoparse import repoparse

class OBSTestReport(TestReport):
    _type = "OBS"

    def __init__(self, *a, **kw):
        super(OBSTestReport, self).__init__(*a, **kw)

        self.rrid = None
        self.rating = None

        self._attrs += [
            'rrid',
            'rating',
        ]

    @property
    def id(self):
        return self.rrid

    def _get_updater_id(self):
        return self.get_release()

    def _parser(self):
        return OBSMetadataParser()

    def _update_repos_parser(self):
        # TODO: exceptions handling
        return repoparse(self.config.template_dir, str(self.id))

    def _show_yourself_data(self):
        return [
            ('ReviewRequestID', self.rrid),
            ('Rating', self.rating),
        ] + super(OBSTestReport, self)._show_yourself_data()

    def set_repo(self, target, operation):
        if operation == 'add':
            target.run_repose('issue-add', self.report_wd())
        elif operation == 'remove':
            target.run_repose('issue-rm', str(self.rrid))
        else:
            raise ValueError(
                "Not supported repose operation {}".format(operation))
