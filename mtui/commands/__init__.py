# -*- coding: utf-8 -*-


import importlib
from pathlib import Path
from mtui.commands._command import Command # noqa W0611

_rootdir = Path(__file__).resolve().parent

cmd_list = []

for pth in _rootdir.glob("*.py"):
    if pth.is_file():
        modname = pth.name[:-3]
    else:
        continue

    # skip things like __init__, __pycache__, __main__ , _commad ...
    if modname.startswith("_"):
        continue
    try:
        module = importlib.import_module("." + modname, 'mtui.commands')
    except BaseException:
        continue

    # register classes
    klzs = [x for x in dir(module) if hasattr(getattr(module, x), "command")]
    cmd_list += klzs

    for x in klzs:
        globals()[x] = getattr(module, x)
