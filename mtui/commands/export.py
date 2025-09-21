"""The `export` command."""

from logging import DEBUG, getLogger
from pathlib import Path

from mtui.argparse import ArgumentParser
from mtui.export.base import BaseExport

from . import Command
from ..export import AutoExport, KernelExport, ManualExport
from ..types.filelist import FileList
from ..utils import complete_choices_filelist, requires_update

logger = getLogger("mtui.commands.export")


class Export(Command):
    """Exports the gathered update data to a template file.

    This includes the pre/post package versions and the update log.
    An output file can be specified; if none is specified, the output
    is written to the current testing template.

    To export a specific updatelog, provide the hostname as a parameter.
    """

    command = "export"

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

    @requires_update
    def __call__(self) -> None:
        """Executes the `export` command."""
        targets: list[str] = list(self.parse_hosts().keys())
        filename = (
            self.args.filename if self.args.filename else Path(self.metadata.path)
        )
        exporters: dict[tuple[bool, bool], type[BaseExport]] = {
            (True, False): AutoExport,
            (False, True): KernelExport,
            (False, False): ManualExport,
        }
        exporter = exporters[(self.config.auto, self.config.kernel)]

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
            except Exception as e:
                if logger.getEffectiveLevel() == DEBUG:
                    logger.exception("traceback of export")
                logger.error(f"While exporting template was thrown exception {e}")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        clist: list[tuple[str, ...]] = [("-f", "--force"), ("-t", "--target")]
        return complete_choices_filelist(clist, line, text, state["hosts"].names())
