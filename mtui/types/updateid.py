from abc import ABCMeta, abstractmethod
from errno import ENOENT
from logging import getLogger

from ..connector.openqa import AutoOpenQA, KernelOpenQA
from ..connector.smelt import SMELT
from ..template import _TemplateIOError, testreport_svn_checkout
from ..template.obstestreport import OBSTestReport
from ..types.obs import RequestReviewID

logger = getLogger("mtui.types.updateid")


class UpdateID(metaclass=ABCMeta):
    def __init__(self, id_, testreport_factory, testreport_svn_checkout):
        self.id = id_
        self.smelt = None
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def _checkout(self, config):
        tr = self.testreport_factory(config)
        trpath = config.template_dir / str(self.id) / "log"

        try:
            tr.read(trpath)
        except _TemplateIOError as e:
            if e.errno != ENOENT:
                raise

            self._vcs_checkout(config, config.svn_path, str(self.id))

            tr.read(trpath)
        return tr

    def _create_installogs_dir(self, config):
        directory = config.template_dir / str(self.id) / config.install_logs
        directory.mkdir(parents=False, exist_ok=True)

    @abstractmethod
    def make_testreport(self, config, autoconnect=True):
        pass


class AutoOBSUpdateID(UpdateID):
    kind = "auto"
    def __init__(self, rrid, *args, **kw):
        super().__init__(RequestReviewID(rrid), OBSTestReport, testreport_svn_checkout)

    def make_testreport(self, config, autoconnect=True):
        tr = self._checkout(config)
        self._create_installogs_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)
        logger.info("Getting data from openQA")
        tr.openqa["auto"] = AutoOpenQA(
            config, config.openqa_instance, tr.smelt, self.id
        ).run()
        # TODO: openQA rework

        if tr.openqa["auto"].results is None:
            logger.warning("No install jobs or install jobs failed")
            logger.info("Switch mode to manual")
            tr.config.auto = False
            if autoconnect:
                logger.info("Connect refhosts from testreport")
                tr.connect_targets()
                for tp in tr.testplatforms:
                    logger.debug("Testplatform: {}".format(tp))
                    tr.refhosts_from_tp(tp)
                logger.info("Connect refhosts from TestPlatform")
                tr.connect_targets()
        tr.updateid = self
        return tr


class KernelOBSUpdateID(UpdateID):
    kind = "kernel"
    def __init__(self, rrid, *args, **kw):
        super().__init__(RequestReviewID(rrid), OBSTestReport, testreport_svn_checkout)

    def create_results_dir(self, config):
        directory = config.template_dir / str(self.id) / "results"
        directory.mkdir(parents=False, exist_ok=True)

    def make_testreport(self, config, autoconnect=False):
        tr = self._checkout(config)
        self._create_installogs_dir(config)
        self.create_results_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)
        tr.updateid = self
        openqa = AutoOpenQA(config, config.openqa_instance, tr.smelt, self.id)
        openqa.run()
        tr.openqa["auto"] = openqa
        kernel = KernelOpenQA(config, config.openqa_instance, tr.smelt, self.id).run()
        baremetal = KernelOpenQA(
            config, config.openqa_instance_baremetal, tr.smelt, self.id
        ).run()
        tr.openqa["kernel"] = [kernel, baremetal]

        return tr
