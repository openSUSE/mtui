"""Classes for handling different types of update IDs."""

import shutil
from abc import ABC, abstractmethod
from collections.abc import Callable
from errno import ENOENT
from logging import getLogger
from pathlib import Path
from typing import TYPE_CHECKING, final, override

if TYPE_CHECKING:
    from ..cli.prompter import Prompter

from ..cli.term import prompt_user
from ..data_sources.openqa import KernelOpenQA
from ..data_sources.qem_dashboard import DashboardAutoOpenQA, QEMIncident
from ..data_sources.teregen import TeReGen
from ..support.config import Config
from ..support.exceptions import (
    FailedGiteaCallError,
    InvalidGiteaHashError,
    MissingGiteaTokenError,
)
from ..support.http import resolve_verify
from ..support.messages import (
    SvnCheckoutFailed,
    SvnCheckoutInterruptedError,
    TestReportNotLoadedError,
)
from ..test_reports.null_report import NullTestReport
from ..test_reports.obs_report import OBSTestReport
from ..test_reports.pi_report import PITestReport
from ..test_reports.sl_report import SLTestReport
from ..test_reports.svn_io import TemplateIOError, testreport_svn_checkout
from ..test_reports.testreport import TestReport
from . import RequestReviewID
from .enums import RequestKind, Workflow

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

    def _checkout(
        self,
        config: Config,
        interactive: bool,
        prompter: "Prompter | None" = None,
    ) -> TestReport:
        """Checks out a test report from version control.

        Args:
            config: The application configuration.
            interactive: Whether to prompt the user for confirmation on
                template hash mismatches.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` forwarded
                to the constructed :class:`TestReport` so that SSH
                command-timeout prompts can reach the user with
                cross-thread serialisation.

        Returns:
            A `TestReport` instance.

        """
        tr = self.testreport_factory(config, prompter=prompter)
        trdir: Path = config.template_dir / str(self.id)
        trpath: Path = trdir / "log"

        try:
            try:
                tr.read(trpath)
            except TemplateIOError as e:
                if e.errno != ENOENT:
                    raise
                try:
                    self._vcs_checkout(config, config.svn_path, self.id)
                except (SvnCheckoutInterruptedError, SvnCheckoutFailed) as e:
                    logger.error(e)
                    raise TestReportNotLoadedError from e
                # Retry the read now that the template is on disk. Any
                # Gitea-related error raised here must reach the outer
                # except clauses below, so we let it propagate.
                tr.read(trpath)
        except MissingGiteaTokenError:
            logger.error(
                "Gitea API token is not configured. "
                "Pass -g/--gitea_token, set GITEA_TOKEN in your environment, "
                "or add a [gitea] token entry to ~/.mtuirc."
            )
            raise
        except FailedGiteaCallError:
            logger.error("Gitea API call failed")
            logger.warning("TestReport isn't loaded")
            raise TestReportNotLoadedError from None

        except InvalidGiteaHashError:
            logger.error("Invalid Gitea hash")
            logger.warning(
                "TestReport hash differs from the Gitea PR; the template is stale"
            )
            teregen = TeReGen(config)
            if prompt_user(
                "Regenerate the template now via TeReGen? [y/N]: ",
                ["yes", "y"],
                interactive,
            ):
                regenerated = self._regenerate(config, teregen, trdir, trpath, prompter)
                if regenerated is not None:
                    return regenerated
                logger.warning("Regeneration failed; falling back to manual handling")
            else:
                logger.info(
                    "TestReport can be regenerated here: https://qam.suse.de/reports/%s/log",
                    self.id,
                )

            if not prompt_user(
                "Force continue loading template ? [y/N]: ", ["yes", "y"], interactive
            ):
                if trdir.exists() and prompt_user(
                    f"Delete checked out template {trdir}? [Y/n]: ",
                    ["yes", "y"],
                    interactive,
                    default=True,
                ):
                    shutil.rmtree(trdir, ignore_errors=True)
                    logger.info("Removed checked out template %s", trdir)
                raise TestReportNotLoadedError from None
            logger.warning("Template is loaded, but hash differs")

        return tr

    def _regenerate(
        self,
        config: Config,
        teregen: TeReGen,
        trdir: Path,
        trpath: Path,
        prompter: "Prompter | None",
    ) -> TestReport | None:
        """Regenerate a stale template via TeReGen, then re-check it out.

        Triggers a server-side regeneration (overwriting the stale template),
        deletes the local checkout, waits for the Minion job to finish, then
        checks out and reads the fresh template. Returns the loaded
        :class:`TestReport` on success, or ``None`` so the caller can fall back
        to the manual force/decline handling.
        """
        logger.info("Waiting for the template to be regenerated ...")
        outcome = teregen.regenerate_and_wait(self.id, force_overwrite=True)
        if outcome.unreachable:
            logger.error("TeReGen unreachable; cannot regenerate")
            return None
        if outcome.error:
            logger.error("Regeneration refused: %s", outcome.error)
            return None
        # The job was accepted: now it is safe to drop the stale local checkout.
        logger.info("Regeneration job %s enqueued for %s", outcome.job, self.id)
        if trdir.exists():
            shutil.rmtree(trdir, ignore_errors=True)
            logger.info("Removed stale checked out template %s", trdir)
        if not outcome.ok:
            logger.error(
                "Regeneration did not finish (state=%s)%s",
                outcome.state or "unknown",
                f": {outcome.minion_error}" if outcome.minion_error else "",
            )
            return None

        tr = self.testreport_factory(config, prompter=prompter)
        try:
            self._vcs_checkout(config, config.svn_path, self.id)
            tr.read(trpath)
        except (
            SvnCheckoutInterruptedError,
            SvnCheckoutFailed,
            TemplateIOError,
            InvalidGiteaHashError,
            FailedGiteaCallError,
            MissingGiteaTokenError,
        ) as e:
            logger.error("Reload after regeneration failed: %s", e)
            return None

        logger.info("Template for %s regenerated and reloaded", self.id)
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
        self,
        config: Config,
        autoconnect: bool = True,
        interactive: bool = True,
        prompter: "Prompter | None" = None,
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
        if id_.kind is RequestKind.SLFO:
            return SLTestReport
        if id_.kind is RequestKind.PI:
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

    @override
    def make_testreport(
        self,
        config: Config,
        autoconnect: bool = True,
        interactive: bool = True,
        prompter: "Prompter | None" = None,
    ) -> TestReport:
        """Creates a `TestReport` instance for an automatic OBS update.

        Args:
            config: The application configuration.
            autoconnect: Whether to automatically connect to hosts.
            interactive: Whether to prompt the user.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` forwarded
                to the constructed :class:`TestReport`.

        Returns:
            A `TestReport` instance.

        """
        try:
            tr = self._checkout(config, interactive, prompter=prompter)
        except TestReportNotLoadedError:
            return NullTestReport(config, prompter=prompter)

        tr.workflow = Workflow.AUTO

        self._create_installogs_dir(config)
        tr.incident = QEMIncident(
            self.id,
            config.qem_dashboard_api,
            resolve_verify(True, config.ssl_verify),
        )

        logger.info("Getting data from QEM Dashboard")
        tr.openqa.auto = DashboardAutoOpenQA(
            config,
            config.openqa_instance,
            tr.incident,
            self.id,
        ).run()

        if tr.openqa.auto.results is None:
            logger.warning("No install jobs or install jobs failed")
            logger.info("Switch mode to manual")
            tr.workflow = Workflow.MANUAL

            if autoconnect:
                # Defer the actual connect to TestReport.autoconnect(), which
                # the loader calls *after* TemplateRegistry.add wires the host
                # arbiter -- so refhosts_from_tp draws one host per slot
                # (with backup) instead of connecting every candidate.
                tr._autoconnect_pending = True  # noqa: SLF001

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

    @override
    def make_testreport(
        self,
        config: Config,
        autoconnect: bool = False,
        interactive: bool = True,
        prompter: "Prompter | None" = None,
    ) -> TestReport:
        """Creates a `TestReport` instance for a kernel OBS update.

        Args:
            config: The application configuration.
            autoconnect: Whether to automatically connect to hosts.
            interactive: Whether to prompt the user.
            prompter: Optional :class:`mtui.cli.prompter.Prompter` forwarded
                to the constructed :class:`TestReport`.

        Returns:
            A `TestReport` instance.

        """
        try:
            tr = self._checkout(config, interactive, prompter=prompter)
        except TestReportNotLoadedError:
            return NullTestReport(config, prompter=prompter)

        tr.workflow = Workflow.KERNEL

        self._create_installogs_dir(config)
        self.create_results_dir(config)
        tr.incident = QEMIncident(
            self.id,
            config.qem_dashboard_api,
            resolve_verify(True, config.ssl_verify),
        )
        tr.openqa.auto = DashboardAutoOpenQA(
            config,
            config.openqa_instance,
            tr.incident,
            self.id,
        ).run()
        kernel = KernelOpenQA(
            config, config.openqa_instance, tr.incident, self.id
        ).run()
        baremetal = KernelOpenQA(
            config,
            config.openqa_instance_baremetal,
            tr.incident,
            self.id,
        ).run()
        tr.openqa.kernel = [kernel, baremetal]

        return tr
