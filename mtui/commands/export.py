from traceback import format_exc
from itertools import zip_longest
from functools import partial
from pathlib import Path

from mtui.commands import Command
from mtui.utils import complete_choices_filelist
from mtui.utils import requires_update
from mtui.utils import prompt_user


from qamlib.utils import timestamp


class Export(Command):
    """
    Exports the gathered update data to template file. This includes
    the pre/post package versions and the update log. An output file could
    be specified, if none is specified, the output is written to the
    current testing template.

    To export a specific updatelog, provide the hostname as parameter.
    """

    command = "export"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="force overwrite existing template",
        )
        parser.add_argument(
            "filename", nargs="?", type=Path, help="output template file name"
        )
        cls._add_hosts_arg(parser)

        return parser

    def _template_fill(self, xmllog):
        def writer(fn, data):
            if filename.exists() and not self.args.force:
                self.log.warning("file {!s} exists.".format(fn))
                if not prompt_user(
                    "Should I overwrite {!s} (y/N) ".format(fn),
                    ["y", "Y", "yes", "Yes", "YES"],
                    self.prompt.interactive,
                ):
                    fn = fn / "." / timestamp()

            self.log.info("exporting log to {!s}".format(fn))

            try:
                with fn.open(mode="w", encoding="utf-8") as f:
                    f.write("\n".join(line.rstrip() for line in data))
            except IOError as e:
                self.println("Failed to write {}: {}".format(fn, e.strerror))
                return

        # TODO: change all paths to Path like objects..
        filename = (
            self.args.filename if self.args.filename else Path(self.metadata.path)
        )

        try:
            template = self.metadata.generate_templatefile(xmllog)
        except Exception as e:
            self.log.error("Failed to export XML")
            self.log.error(e)
            self.log.debug(format_exc())
            return

        template, smelt = self.metadata.strip_smeltdata(template)

        writer(filename, template)
        self.println("wrote template to {}".format(filename))

        if smelt:
            filename = filename.parent / "checkers.log"
            writer(filename, smelt)
            self.println("wrote checkers results to {}".format(filename))

    def _installlogs_fill(self, xmllog, targets):
        filepath = (
            self.config.template_dir / str(self.metadata.id) / self.config.install_logs
        )
        generator = partial(self.metadata.generate_install_logs, xmllog)

        ilogs = zip_longest(targets, map(generator, targets))

        for i, y in ilogs:
            filename = i + ".log"

            if filepath.joinpath(filename).exists() and not self.args.force:
                self.log.warning("file {!s} exists.".format(filename))
                if not prompt_user(
                    "Should I overwrite {!s} (y/N) ".format(filename),
                    ["y", "Y", "yes", "Yes", "YES"],
                    self.prompt.interactive,
                ):
                    filename += "." + timestamp()
            self.log.info("exporting zypper log from {!s} to {!s}".format(i, filename))

            try:
                with filepath.joinpath(filename).open(mode="w", encoding="utf-8") as f:
                    f.write("\n".join(line.rstrip() for line in y))
            except IOError as e:
                self.println("Failed to write {}: {}".format(filename, e.strerror))

            self.println("wrote zypper log to {}".format(filename))

    @requires_update
    def run(self):
        targets = self.parse_hosts().keys()
        xmllog = self.metadata.generate_xmllog(self.targets.select(targets).values())

        self._template_fill(xmllog)
        self._installlogs_fill(xmllog, targets)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        clist = [("-f", "--force"), ("-t", "--target")]
        return complete_choices_filelist(clist, line, text, state["hosts"].names())
