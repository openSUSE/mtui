from abc import ABC, abstractmethod
from errno import ENOENT
from logging import getLogger
from pathlib import Path
from typing import Callable, Type

from . import RequestReviewID
from ..config import Config
from ..connector import SMELT
from ..connector.openqa import AutoOpenQA, KernelOpenQA
from ..messages import (
    SvnCheckoutFailed,
    SvnCheckoutInterruptedError,
    TestReportNotLoadedError,
)
from ..template import TemplateIOError, testreport_svn_checkout
from ..template.nulltestreport import NullTestReport
from ..template.obstestreport import OBSTestReport

logger = getLogger("mtui.types.updateid")


class UpdateID(ABC):
    def __init__(
        self,
        id_: RequestReviewID,
        # testreport factory ... we have only one type of testreport now
        testreport_factory: Type[OBSTestReport],
        testreport_svn_checkout: Callable[[Config, str, RequestReviewID], None],
    ) -> None:
        self.id = id_
        self.smelt: SMELT
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def _checkout(self, config: Config) -> OBSTestReport:
        tr = self.testreport_factory(config)
        trpath: Path = config.template_dir / str(self.id) / "log"

        try:
            tr.read(trpath)
        except TemplateIOError as e:
            if e.errno != ENOENT:
                raise
            try:
                self._vcs_checkout(config, config.svn_path, self.id)  # type: ignore
            except (SvnCheckoutInterruptedError, SvnCheckoutFailed) as e:
                logger.error(e)
                raise TestReportNotLoadedError
            else:
                tr.read(trpath)

        return tr

    def _create_installogs_dir(self, config) -> None:
        directory: Path = config.template_dir / str(self.id) / config.install_logs
        directory.mkdir(parents=False, exist_ok=True)

    @abstractmethod
    def make_testreport(
        self, config: Config, autoconnect: bool = True
    ) -> OBSTestReport | NullTestReport: ...


class AutoOBSUpdateID(UpdateID):
    kind = "auto"

    def __init__(self, rrid: str, *args, **kwds) -> None:
        super().__init__(RequestReviewID(rrid), OBSTestReport, testreport_svn_checkout)

    def make_testreport(
        self, config: Config, autoconnect: bool = True
    ) -> OBSTestReport | NullTestReport:
        try:
            tr = self._checkout(config)
        except TestReportNotLoadedError:
            return NullTestReport(config)

        self._create_installogs_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)  # type: ignore

        logger.info("Getting data from openQA")
        tr.openqa["auto"] = AutoOpenQA(
            config,
            config.openqa_instance,  # type: ignore
            tr.smelt,
            self.id,
        ).run()

        if tr.openqa["auto"].results is None:
            logger.warning("No install jobs or install jobs failed")
            logger.info("Switch mode to manual")
            tr.config.auto = False  # type: ignore

            if autoconnect:
                logger.info("Connect refhosts from testreport")
                tr.connect_targets()

                for tp in tr.testplatforms:
                    logger.debug("Testplatform: %s", tp)
                    tr.refhosts_from_tp(tp)

                logger.info("Connect refhosts from TestPlatform")
                tr.connect_targets()

        tr.updateid = self  # type: ignore
        return tr


class KernelOBSUpdateID(UpdateID):
    kind = "kernel"

    def __init__(self, rrid: str, *args, **kw) -> None:
        super().__init__(RequestReviewID(rrid), OBSTestReport, testreport_svn_checkout)

    def create_results_dir(self, config: Config) -> None:
        directory: Path = config.template_dir / str(self.id) / "results"
        directory.mkdir(parents=False, exist_ok=True)

    def make_testreport(
        self, config: Config, autoconnect: bool = False
    ) -> NullTestReport | OBSTestReport:
        try:
            tr = self._checkout(config)
        except TestReportNotLoadedError:
            return NullTestReport(config)

        self._create_installogs_dir(config)
        self.create_results_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)
        tr.updateid = self  # type: ignore
        tr.openqa["auto"] = AutoOpenQA(
            config,
            config.openqa_instance,  # type: ignore
            tr.smelt,
            self.id,
        ).run()  # type: ignore
        kernel = KernelOpenQA(config, config.openqa_instance, tr.smelt, self.id).run()  # type: ignore
        baremetal = KernelOpenQA(
            config,
            config.openqa_instance_baremetal,  # type: ignore
            tr.smelt,
            self.id,  # type: ignore
        ).run()
        tr.openqa["kernel"] = [kernel, baremetal]

        return tr
