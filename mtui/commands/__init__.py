"""This package contains the command modules for mtui.

Each module in this package defines a specific command that can be
executed from the mtui command prompt. The modules are dynamically
imported and registered as commands.
"""

import importlib
from logging import getLogger
from pathlib import Path

from ._command import Command as Command

logger = getLogger("mtui.commands")

_rootdir = Path(__file__).resolve().parent
cmd_list: list[str] = []

for pth in _rootdir.glob("*.py"):
    if pth.is_file():
        modname = pth.name[:-3]
    else:
        continue

    # skip things like __init__, __pycache__, __main__ , _commad ...
    if modname.startswith("_"):
        continue
    try:
        logger.debug("loading command module %s", modname)
        module = importlib.import_module("." + modname, "mtui.commands")
    except BaseException:
        logger.error("loading command module %s failed", modname)
        continue

    # register classes
    klzs = [x for x in dir(module) if hasattr(getattr(module, x), "command")]
    cmd_list += klzs

    for x in klzs:
        globals()[x] = getattr(module, x)
