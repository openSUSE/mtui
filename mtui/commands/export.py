"""The `export` command."""

from logging import getLogger
from pathlib import Path

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices_filelist, template_completion
from ..data_sources.qem_dashboard import DashboardAutoOpenQA
from ..support.misc import requires_update
from ..types import Workflow
from ..types.filelist import FileList
from ..update_workflow.export import AutoExport, KernelExport, ManualExport
from ..update_workflow.export.base import BaseExport
from . import Command

logger = getLogger("mtui.commands.export")


class Export(Command):
    """Exports the gathered update data to a template file.

    This includes the pre/post package versions and the update log.
    An output file can be specified; if none is specified, the output
    is written to the current testing template.

    To export a specific updatelog, provide the hostname as a parameter.
    """

    command = "export"
    scope = "fanout"

    def _requires_hosts(self, report) -> bool:
        """Export needs a connected host only in the MANUAL workflow.

        AUTO and KERNEL exports build the template from openQA/dashboard data
        and never touch a connected host, so a host-less template must not be
        skipped (nor make an all-host-less fan-out raise
        :class:`NoRefhostsDefinedError`). MANUAL export reports per-host results
        and keeps the host-phase behaviour.
        """
        return report.workflow not in (Workflow.AUTO, Workflow.KERNEL)

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="force overwrite existing template and if openQA results are in log, download them again and rewrite old",
        )
        parser.add_argument(
            "filename", nargs="?", type=Path, help="output template file name"
        )
        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `export` command."""
        targets: list[str] = list(self.parse_hosts().keys())
        filename = self.args.filename or Path(self.metadata.path)
        exporters: dict[Workflow, type[BaseExport]] = {
            Workflow.AUTO: AutoExport,
            Workflow.KERNEL: KernelExport,
            Workflow.MANUAL: ManualExport,
        }
        exporter = exporters[self.metadata.workflow]
        if issubclass(exporter, ManualExport) and not self.metadata.openqa.auto:
            self.metadata.openqa.auto = DashboardAutoOpenQA(
                self.config,
                self.config.openqa_instance,
                self.metadata.incident,
                self.metadata.rrid,
            ).run()

        if issubclass(exporter, ManualExport):
            results = self.metadata.report_results(
                self.targets.select(targets).values()
            )
        else:
            results = []

        with FileList.load(filename) as text:
            try:
                template = exporter(
                    self.config,
                    self.metadata.openqa,
                    text,
                    self.args.force,
                    self.metadata.id,
                    self.prompt.interactive,
                    results=results,
                ).run(targets)
                text.clear()
                text.extend(template)
            except Exception:
                logger.exception("While exporting template was thrown exception")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        clist: list[tuple[str, ...]] = [
            ("-f", "--force"),
            ("-t", "--target"),
            *template_completion(state),
        ]
        return complete_choices_filelist(clist, line, text, state["hosts"].names())
