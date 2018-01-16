
from os.path import join
from errno import ENOENT

from mtui.template import _TemplateIOError
from mtui.template import testreport_svn_checkout
from mtui.template.obstestreport import OBSTestReport

from qamlib.types.obs import RequestReviewID
from qamlib.smelt import SMELT


class UpdateID(object):

    def __init__(self, id_, testreport_factory, testreport_svn_checkout):
        self.id = id_
        self.smelt = None
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def make_testreport(self, config, logger, autoconnect=True):
        tr = self.testreport_factory(
            config,
            logger,
        )
        trpath = join(config.template_dir, str(self.id), 'log')

        try:
            tr.read(trpath)
        except _TemplateIOError as e:
            if e.errno != ENOENT:
                raise

            self._vcs_checkout(
                config,
                logger,
                config.svn_path,
                str(self.id))

            tr.read(trpath)

        if autoconnect:
            tr.connect_targets()

        tr.smelt = self.smelt
        if self.smelt:
            tr.smelt.logger = logger

        return tr


class OBSUpdateID(UpdateID):

    def __init__(self, rrid, *args, **kw):
        super(OBSUpdateID, self).__init__(
            RequestReviewID(rrid),
            OBSTestReport,
            testreport_svn_checkout
        )

        self.smelt = SMELT(self.id.maintenance_id)