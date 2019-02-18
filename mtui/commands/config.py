from mtui.commands import Command


class Config(Command):

    """
    Display and manipulate (TODO) configuration in runtime.
    """

    command = "config"
    _check_subparser = "func"

    @classmethod
    def _add_arguments(cls, p):
        sp = p.add_subparsers()

        p_show = sp.add_parser("show", help="show config values", sys_=p.sys)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")

        p_set = sp.add_parser("set", help="set config values", sys_=p.sys)
        p_set.add_argument("attribute", type=str)
        p_set.add_argument("value", type=str)
        p_set.set_defaults(func="set")

    def run(self):
        getattr(self, self.args.func)()

    def show(self):
        attrs = self.args.attributes
        if not attrs:
            attrs = [x[0] for x in self.config.data]

        max_attr_len = len(max(attrs, key=len))
        for i in attrs:
            fmt = "{0:<" + str(max_attr_len) + "} = {1!r}"
            try:
                self.println(fmt.format(i, getattr(self.config, i)))
            except AttributeError:
                pass

    def set(self):
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
        self.println("option: {} set to value : {}".format(attr, val))
