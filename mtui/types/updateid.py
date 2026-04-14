"""Classes for handling different types of update IDs."""

from abc import ABC, abstractmethod
from collections.abc import Callable
from errno import ENOENT
from logging import getLogger
from pathlib import Path
from typing import final

from mtui.exceptions import (
    FailedGiteaCallError,
    InvalidGiteaHashError,
    MissingGiteaTokenError,
)

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
from ..template.pitestreport import PITestReport
from ..template.sltestreport import SLTestReport
from ..template.testreport import TestReport
from ..utils import prompt_user
from . import RequestReviewID

logger = getLogger("mtui.types.updateid")


class UpdateID(ABC):
    """An abstract base class for all update ID classes."""

    def __init__(
        self,
        id_: RequestReviewID,
        # testreport factory ... we have only one type of testreport now
        testreport_factory: type[TestReport],
        testreport_svn_checkout: Callable[[Config, str, RequestReviewID], None],
    ) -> None:
        """Initializes the `UpdateID` object.

        Args:
            id_: The `RequestReviewID` of the update.
            testreport_factory: The factory for creating `TestReport` instances.
            testreport_svn_checkout: The function for checking out test reports.

        """
        self.id = id_
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def _checkout(self, config: Config, interactive: bool) -> TestReport:
        """Checks out a test report from version control.

        Args:
            config: The application configuration.

        Returns:
            A `TestReport` instance.

        """
        tr = self.testreport_factory(config)
        trpath: Path = config.template_dir / str(self.id) / "log"

        try:
            tr.read(trpath)
        except TemplateIOError as e:
            if e.errno != ENOENT:
                raise
            try:
                self._vcs_checkout(config, config.svn_path, self.id)
            except (SvnCheckoutInterruptedError, SvnCheckoutFailed) as e:
                logger.error("SVN checkout failed")
                raise TestReportNotLoadedError from e
            else:
                try:
                    tr.read(trpath)
                except Exception as e:
                    raise e
        except (MissingGiteaTokenError, FailedGiteaCallError):
            logger.error("Gitea error")
            logger.warning("TestReport ins't loaded")
            raise TestReportNotLoadedError from None

        except InvalidGiteaHashError:
            logger.error("Invalid Gitea hash")
            logger.info(
                "TestReport has different hash than GiteaPR, please regenerate template"
            )
            logger.info(
                "TestReport can be regenerated here: https://qam.suse.de/reports/%s/log",
                self.id,
            )
            if not prompt_user(
                "Force continue loading template ? [Y/N]: ", ["yes", "y"], interactive
            ):
                raise TestReportNotLoadedError from None
            logger.warning("Template is loaded, but hash differs")

        return tr

    def _create_installogs_dir(self, config) -> None:
        """Creates the install logs directory.

        Args:
            config: The application configuration.

        """
        directory: Path = config.template_dir / str(self.id) / config.install_logs
        directory.mkdir(parents=False, exist_ok=True)

    @abstractmethod
    def make_testreport(
        self, config: Config, autoconnect: bool = True, interactive: bool = True
    ) -> TestReport:
        """An abstract method for creating a `TestReport` instance."""
        ...

    @staticmethod
    def tr_factory(id_: RequestReviewID) -> type[TestReport]:
        """A factory function that returns the `TestReport` class for a given ID.

        Args:
            id_: The `RequestReviewID` of the update.

        Returns:
            The `TestReport` class for the given ID.

        """
        if id_.kind == "SLFO":
            return SLTestReport
        if id_.kind == "PI":
            return PITestReport
        return OBSTestReport


@final
class AutoOBSUpdateID(UpdateID):
    """An `UpdateID` implementation for automatic OBS updates."""

    kind = "auto"

    def __init__(self, rrid: str, *args, **kwds) -> None:
        """Initializes the `AutoOBSUpdateID` object.

        Args:
            rrid: The Request Review ID string.
            *args: Additional arguments.
            **kwds: Additional keyword arguments.

        """
        id_ = RequestReviewID(rrid)

        super().__init__(id_, self.tr_factory(id_), testreport_svn_checkout)

    def make_testreport(
        self, config: Config, autoconnect: bool = True, interactive: bool = True
    ) -> TestReport:
        """Creates a `TestReport` instance for an automatic OBS update.

        Args:
            config: The application configuration.
            autoconnect: Whether to automatically connect to hosts.

        Returns:
            A `TestReport` instance.

        """
        try:
            tr = self._checkout(config, interactive)
        except TestReportNotLoadedError:
            return NullTestReport(config)

        self._create_installogs_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)

        logger.info("Getting data from openQA")
        tr.openqa["auto"] = AutoOpenQA(
            config,
            config.openqa_instance,
            tr.smelt,
            self.id,
        ).run()

        if tr.openqa["auto"].results is None:
            logger.warning("No install jobs or install jobs failed")
            logger.info("Switch mode to manual")
            tr.config.auto = False

            if autoconnect:
                logger.info("Connect refhosts from testreport")
                tr.connect_targets()

                for tp in tr.testplatforms:
                    logger.debug("Testplatform: %s", tp)
                    tr.refhosts_from_tp(tp)

                logger.info("Connect refhosts from TestPlatform")
                tr.connect_targets()

        tr.updateid = self
        return tr


@final
class KernelOBSUpdateID(UpdateID):
    """An `UpdateID` implementation for kernel OBS updates."""

    kind = "kernel"

    def __init__(self, rrid: str, *args, **kw) -> None:
        """Initializes the `KernelOBSUpdateID` object.

        Args:
            rrid: The Request Review ID string.
            *args: Additional arguments.
            **kw: Additional keyword arguments.

        """
        id_ = RequestReviewID(rrid)
        super().__init__(id_, self.tr_factory(id_), testreport_svn_checkout)

    def create_results_dir(self, config: Config) -> None:
        """Creates the results directory.

        Args:
            config: The application configuration.

        """
        directory: Path = config.template_dir / str(self.id) / "results"
        directory.mkdir(parents=False, exist_ok=True)

    def make_testreport(
        self, config: Config, autoconnect: bool = False, interactive: bool = True
    ) -> TestReport:
        """Creates a `TestReport` instance for a kernel OBS update.

        Args:
            config: The application configuration.
            autoconnect: Whether to automatically connect to hosts.

        Returns:
            A `TestReport` instance.

        """
        try:
            tr = self._checkout(config, interactive)
        except TestReportNotLoadedError:
            return NullTestReport(config)

        self._create_installogs_dir(config)
        self.create_results_dir(config)
        tr.smelt = SMELT(self.id, config.smelt_api)
        tr.updateid = self
        tr.openqa["auto"] = AutoOpenQA(
            config,
            config.openqa_instance,
            tr.smelt,
            self.id,
        ).run()
        kernel = KernelOpenQA(config, config.openqa_instance, tr.smelt, self.id).run()
        baremetal = KernelOpenQA(
            config,
            config.openqa_instance_baremetal,
            tr.smelt,
            self.id,
        ).run()
        tr.openqa["kernel"] = [kernel, baremetal]

        return tr
