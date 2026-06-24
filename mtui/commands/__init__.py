"""This package contains the command modules for mtui.

Each module in this package defines one or more :class:`Command` subclasses.
Importing this package walks the package directory with
:func:`pkgutil.iter_modules`, imports every non-underscore submodule, and
relies on :meth:`Command.__init_subclass__` to populate
:data:`Command.registry` as a side-effect of class creation. The registry is
re-exported as :data:`registry` for use by ``mtui.prompt``.
"""

import importlib
import pkgutil
from logging import getLogger

from ._command import Command as Command
from ._command import CommandAlreadyBoundError as CommandAlreadyBoundError

logger = getLogger("mtui.commands")

# Import every submodule whose name does not start with ``_`` so that each
# Command subclass triggers Command.__init_subclass__. We do not need the
# returned module objects; the side-effect of import is the whole point.
for _modinfo in pkgutil.iter_modules(__path__):
    if _modinfo.name.startswith("_"):
        continue
    try:
        logger.debug("loading command module %s", _modinfo.name)
        importlib.import_module(f".{_modinfo.name}", __name__)
    except Exception:
        logger.exception("loading command module %s failed", _modinfo.name)
        continue

#: Public alias for :attr:`Command.registry`. Keys are the user-facing command
#: strings (e.g. ``"set_location"``), values are the concrete
#: :class:`Command` subclasses.
registry = Command.registry
