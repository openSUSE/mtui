"""The `put` and `get` commands for SFTP file transfers."""

import os
from glob import glob
from logging import getLogger
from pathlib import Path

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices_filelist, template_completion
from . import Command

logger = getLogger("mtui.command.sftp")


class SFTPPut(Command):
    """Uploads files to all enabled hosts.

    Multiple files can be selected with special patterns according to the
    rules used by the Unix shell (e.g., *, ?, []). The complete
    filepath on the remote hosts is shown after the upload.
    """

    command = "put"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)
        parser.add_argument(
            "filename", nargs=1, type=str, help="file to upload to all hosts"
        )

    def __call__(self) -> None:
        """Executes the `put` command."""
        files: list[Path] = [Path(f) for f in glob(self.args.filename[0])]
        if not files:
            logger.error("File %s not found", self.args.filename[0])
            return

        # (local path, path relative to the upload root) pairs: a walked
        # directory keeps its tree on the remote side. Flattening to the
        # basename made d/a/config and d/b/config both land on the same
        # remote path, silently clobbering the first with the second.
        transversed_files: list[tuple[Path, Path]] = []
        for file in files:
            if file.is_file():
                transversed_files.append((file, Path(file.name)))
            elif file.is_dir():
                # Resolve before walking: a '..'-shaped argument (put ..)
                # would otherwise survive relative_to() as a literal '..'
                # part and build a remote path that escapes the run's
                # working directory into the shared target_tempdir.
                walk_root = file.resolve()
                # Path.walk is from 3.12
                for root, _, folder_files in os.walk(walk_root):
                    transversed_files.extend(
                        (
                            Path(root) / folder_file,
                            (Path(root) / folder_file).relative_to(walk_root.parent),
                        )
                        for folder_file in folder_files
                    )
            else:
                logger.warning("Filename %s isn't file", file)
                continue

        # work only on enabled hosts
        targets = self.targets.select(enabled=True)
        for filename, relative in transversed_files:
            # sftp_put creates the intermediate remote directories.
            remote = self.metadata.target_wd(*relative.parts)

            targets.sftp_put(filename, remote)
            logger.info("uploaded %s to %s", filename, remote)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices_filelist(list(template_completion(state)), line, text)


class SFTPGet(Command):
    """Downloads a file from all enabled hosts.

    Multiple files cannot be selected. Files are saved in the
    ${TEMPLATE_DIR}/downloads/ subdirectory with the hostname as a file
    extension. If the argument ends with a slash '/', it will be
    treated as a folder and all its contents will be downloaded.
    """

    command = "get"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)
        parser.add_argument(
            "filename", nargs=1, type=Path, help="file to download from target hosts"
        )

    def __call__(self) -> None:
        """Executes the `get` command."""
        # Work only on enabled hosts, matching `put` and the docstring (a
        # disabled host has been deliberately parked and must not be contacted).
        targets = self.targets.select(enabled=True)
        self.metadata.perform_get(targets, self.args.filename[0])
        logger.info("downloaded %s", self.args.filename[0])

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices_filelist(list(template_completion(state)), line, text)
