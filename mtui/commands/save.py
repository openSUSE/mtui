# -*- coding: utf-8 -*-


from mtui.commands import Command


class DoSave(Command):
    """
    Save the testing log to a XML file. All commands and package
    versions are saved there. When no parameter is given, the XML is saved
    to $TEMPLATE_DIR/output/log.xml. If that file already exists and the
    tester doesn't want to overwrite it, a postfix (current timestamp)
    is added to the filename.
    """
    command = 'save'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'filename', default='log.xml', nargs='?',
            help='save log as file filename')
        return parser

    def run(self):
        self.prompt._do_save_impl(self.args.filename)
