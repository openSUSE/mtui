# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

def make_cp(config = None, logger = None, sys = None, display = None):
  from mtui.display import CommandPromptDisplay
  from mtui.prompt import CommandPrompt
  from tests.utils import ConfigFake, LogFake, SysFake
  return CommandPrompt(
    config or ConfigFake(),
    logger or LogFake(),
    sys or SysFake(),
    display or CommandPromptDisplay,
  )
