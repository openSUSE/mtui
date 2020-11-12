import os
from glob import glob
from logging import getLogger

from mtui.commands import Command
from mtui.utils import complete_choices_filelist

logger = getLogger("mtui.command.sftp")


class SFTPPut(Command):
    """
    Uploads files to all enabled hosts.
    Multiple files can be selected with special patterns according to the rules
    used by the Unix shell (i.e. *, ?, []). The complete filepath on the remote
    hosts is shown after the upload.
    """

    command = "put"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "filename", nargs=1, type=str, help="file to upload to all hosts"
        )

    def __call__(self):
        files = glob(self.args.filename[0])
        if not files:
            logger.error("File {!s} not found".format(self.args.filename[0]))
            return

        transversed_files = []
        for file in files:
            if os.path.isfile(file):
                transversed_files.append(file)
            elif os.path.isdir(file):
                for root, _, folder_files in os.walk(file):
                    for folder_file in folder_files:
                        transversed_files.append(os.path.join(root, folder_file))
            else:
                logger.warn("Filename {!s} isn't file".format(file))
                continue

        for filename in transversed_files:
            remote = self.metadata.target_wd(os.path.basename(filename))

            self.targets.put(filename, remote)
            logger.info("uploaded {} to {}".format(filename, remote))

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices_filelist([], line, text)


class SFTPGet(Command):
    """
    Downloads a file from all enabled hosts.
    Multiple files cannot be selected.
    Files are saved in the ${TEMPLATE_DIR}/downloads/ subdirectory
    with the hostname as file extension. If the argument ends with a slash '/',
    it will be treated as a folder and all its contents will be downloaded.
    """

    command = "get"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "filename", nargs=1, help="file to download from target hosts"
        )

    def __call__(self):
        self.metadata.perform_get(self.targets, self.args.filename[0])
        logger.info("downloaded {}".format(self.args.filename[0]))
