"""The `config` command."""

from mtui.argparse import ArgumentParser
from mtui.commands import Command


class Config(Command):
    """Displays and manipulates the configuration at runtime."""

    command = "config"
    _check_subparser = "func"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        sp = parser.add_subparsers()

        p_show = sp.add_parser("show", help="show config values", sys_=parser.sys)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")

        p_set = sp.add_parser("set", help="set config values", sys_=parser.sys)
        p_set.add_argument("attribute", type=str)
        p_set.add_argument("value", type=str)
        p_set.set_defaults(func="set")

    def __call__(self) -> None:
        """Executes the appropriate subcommand (`show` or `set`)."""
        getattr(self, self.args.func)()

    def show(self):
        """Displays the current configuration values."""
        attrs = self.args.attributes
        if not attrs:
            attrs = sorted(x[0] for x in self.config.data)

        max_attr_len = len(max(attrs, key=len))
        for i in attrs:
            fmt = "{0:<" + str(max_attr_len) + "} = {1!r}"
            try:
                self.println(fmt.format(i, getattr(self.config, i)))
            except AttributeError:
                pass

    def set(self) -> None:
        """Sets a configuration value."""
        attr = self.args.attribute
        val = self.args.value

        try:
            typ = type(getattr(self.config, attr))
        except AttributeError:
            if val in ("True", "False"):
                typ = bool
            elif val.isdigit():
                typ = int
            else:
                typ = str

        if typ is bool:
            if val == "True":
                val = True
            else:
                val = False
        else:
            val = typ(val)

        setattr(self.config, attr, val)
        self.println(f"option: {attr} set to value : {val}")
