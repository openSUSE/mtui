# -*- coding: utf-8 -*-

import os

from glob import glob

from mtui.commands import Command
from mtui.utils import complete_choices_filelist


class SFTPPut(Command):
    """
    Uploads files to all enabled hosts. Multiple files can be selected
    with special patterns according to the rules used by the Unix shell
    (i.e. *, ?, []). The complete filepath on the remote hosts is shown
    after the upload.
    """
    command = 'put'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'filename',
            nargs=1,
            help='file to upload to a hosts')
        return parser

    def run(self):
        for filename in glob(self.args.filename[0]):
            if not os.path.isfile(filename):
                continue

            remote = self.metadata.target_wd(os.path.basename(filename))

            self.targets.put(filename, remote)
            self.log.info('uploaded {} to {}'.format(filename, remote))

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices_filelist([], line, text)


class SFTPGet(Command):
    """
    Downloads a file from all enabled hosts. Multiple files cannot be
    selected. Files are saved in the $TEMPLATE_DIR/downloads/ subdirectory
    with the hostname as file extension. If the argument ends with a
    slash '/', it will be treated as a folder and all its contents will
    be downloaded.
    """
    command = 'get'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'filename',
            nargs=1,
            help='file to download from target hosts')
        return parser

    def run(self):
        self.metadata.perform_get(self.targets, self.args.filename[0])
        self.log.info('downloaded {}'.format(self.args.filename[0]))
