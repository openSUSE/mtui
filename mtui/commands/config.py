"""The `config` command."""

import configparser
from contextlib import suppress
from logging import getLogger
from typing import Any

from ..cli.argparse import ArgumentParser
from ..support.config import ConfigOption
from . import Command

logger = getLogger("mtui.commands.config")

#: Accepted spellings for boolean values -- the exact table
#: :meth:`configparser.ConfigParser.getboolean` uses to parse INI values
#: (``1``/``yes``/``true``/``on`` and ``0``/``no``/``false``/``off``).
_BOOLEAN_STATES = configparser.ConfigParser.BOOLEAN_STATES


def _parse_option_value(opt: ConfigOption, raw: str) -> Any:
    """Coerce ``raw`` the way config-file parsing would.

    Emulates the option's declared ``getter`` (``getint`` parses an
    integer, ``getboolean`` parses configparser's boolean spellings,
    anything else keeps the string as-is), then applies the option's
    ``fixup`` -- the same pipeline :meth:`Config._parse_config` runs on
    INI values, so runtime `config set` cannot store a value the config
    file would have rejected.

    Raises:
        ValueError: ``raw`` cannot be parsed by the getter semantics or
            is rejected by the option's ``fixup``.

    """
    getter_name = getattr(opt.getter, "__name__", "")
    value: Any = raw
    if getter_name == "getint":
        try:
            value = int(raw)
        except ValueError:
            raise ValueError("expected an integer") from None
    elif getter_name == "getboolean":
        try:
            value = _BOOLEAN_STATES[raw.lower()]
        except KeyError:
            raise ValueError(
                "expected a boolean (one of: 1, yes, true, on, 0, no, false, off)"
            ) from None
    try:
        return opt.fixup(value)
    except ValueError:
        raise
    except Exception as e:
        # Fixups are arbitrary callables; normalize any failure so the
        # caller has a single rejection path.
        raise ValueError(str(e)) from e


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
            attrs = sorted(opt.attr for opt in self.config.data)

        max_attr_len = len(max(attrs, key=len))
        for i in attrs:
            fmt = "{0:<" + str(max_attr_len) + "} = {1!r}"
            with suppress(AttributeError):
                self.println(fmt.format(i, getattr(self.config, i)))

    def set(self) -> None:
        """Sets a configuration value.

        Attributes backed by a declared :class:`ConfigOption` go through
        the same getter/fixup pipeline as config-file parsing, so e.g.
        ``config set use_keyring true`` and ``config set ssl_verify
        false`` behave exactly like their INI counterparts. An invalid
        value is rejected with an error and the attribute is left
        unchanged. Attributes without a declared option (set externally,
        e.g. ``distro``, or brand new) are coerced via the current
        attribute type as before.
        """
        attr: str = self.args.attribute
        raw: str = self.args.value

        opt = next(
            (o for o in getattr(self.config, "data", []) if o.attr == attr), None
        )
        if opt is not None:
            try:
                val = _parse_option_value(opt, raw)
            except ValueError as e:
                logger.error("config: cannot set %s to %r: %s", attr, raw, e)
                return
        else:
            try:
                typ = type(getattr(self.config, attr))
            except AttributeError:
                if raw in ("True", "False"):
                    typ = bool
                elif raw.isdigit():
                    typ = int
                else:
                    typ = str
            try:
                val = _BOOLEAN_STATES[raw.lower()] if typ is bool else typ(raw)
            except (KeyError, ValueError):
                logger.error(
                    "config: cannot set %s to %r: expected a value of type %s",
                    attr,
                    raw,
                    typ.__name__,
                )
                return

        setattr(self.config, attr, val)
        self.println(f"option: {attr} set to value : {val}")
