"""The `put` and `get` commands for SFTP file transfers."""

from glob import glob
from logging import getLogger
import os
from pathlib import Path

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices_filelist

logger = getLogger("mtui.command.sftp")


class SFTPPut(Command):
    """Uploads files to all enabled hosts.

    Multiple files can be selected with special patterns according to the
    rules used by the Unix shell (e.g., *, ?, []). The complete
    filepath on the remote hosts is shown after the upload.
    """

    command = "put"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "filename", nargs=1, type=str, help="file to upload to all hosts"
        )

    def __call__(self) -> None:
        """Executes the `put` command."""
        files: list[Path] = [Path(f) for f in glob(self.args.filename[0])]
        if not files:
            logger.error("File %s not found", self.args.filename[0])
            return

        transversed_files = []
        for file in files:
            if file.is_file():
                transversed_files.append(file)
            elif file.is_dir():
                # Path.walk is from 3.12
                for root, _, folder_files in os.walk(file):
                    for folder_file in folder_files:
                        transversed_files.append(Path(root) / folder_file)
            else:
                logger.warning("Filename %s isn't file", file)
                continue

        # work only on enabled hosts
        targets = self.targets.select(enabled=True)
        for filename in transversed_files:
            remote = self.metadata.target_wd(filename.name)

            targets.sftp_put(filename, remote)
            logger.info("uploaded %s to i%s", filename, remote)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices_filelist([], line, text)


class SFTPGet(Command):
    """Downloads a file from all enabled hosts.

    Multiple files cannot be selected. Files are saved in the
    ${TEMPLATE_DIR}/downloads/ subdirectory with the hostname as a file
    extension. If the argument ends with a slash '/', it will be
    treated as a folder and all its contents will be downloaded.
    """

    command = "get"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "filename", nargs=1, type=Path, help="file to download from target hosts"
        )

    def __call__(self) -> None:
        """Executes the `get` command."""
        self.metadata.perform_get(self.targets, self.args.filename[0])
        logger.info("downloaded %s", self.args.filename[0])
