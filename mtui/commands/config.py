# -*- coding: utf-8 -*-

from mtui.commands import Command


class Config(Command):

    """
    Display and manipulate (TODO) configuration in runtime.
    """
    command = "config"
    _check_subparser = "func"

    def run(self):
        getattr(self, self.args.func)()

    def show(self):
        attrs = self.args.attributes
        if not attrs:
            attrs = [x[0] for x in self.config.data]

        max_attr_len = len(max(attrs, key=len))
        for i in attrs:
            fmt = "{0:<" + str(max_attr_len) + "} = {1!r}"
            self.println(fmt.format(i, getattr(self.config, i)))

    @classmethod
    def _add_arguments(cls, p):
        sp = p.add_subparsers()
        p_show = sp.add_parser("show", help="show config values",
                               sys_=p.sys)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")
