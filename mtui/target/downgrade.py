# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function

import re

from mtui.rpmver import RPMVersion
from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue
from mtui.target.actions import spinner


class Downgrade(object):
  def __init__(self, logger, targets, packages, patches):
    self.log = logger
    self.targets = targets
    self.packages = packages
    self.patches = patches

    self.commands = {}
    self.install_command = None
    self.list_command = None
    self.pre_commands = []
    self.post_commands = []

  def run(self):
    skipped = False
    versions = {}

    try:
      for t in self.targets.values():
        lock = t.locked()
        if lock.locked and not lock.own():
          skipped = True
          self.log.warning('host %s is locked since %s by %s. skipping.' % (t.hostname, lock.time(), lock.user))
          if lock.comment:
            self.log.info("%s's comment: %s" % (lock.user, lock.comment))
        else:
          t.set_locked()
          thread = ThreadedMethod(queue)
          thread.setDaemon(True)
          thread.start()

      if skipped:
        for t in self.targets.values():
          try:
            t.remove_lock()
          except AssertionError:
            pass
        raise UpdateError('Hosts locked')

      for t in self.targets.values():
        queue.put([t.set_repo, ['UPDATE']])

      while queue.unfinished_tasks:
        spinner()

      queue.join()

      for t in self.targets.values():
        if t.lasterr():
          self.log.critical('failed to downgrade host %s. stopping.\n# %s\n%s' % (t.hostname, t.lastin(), t.lasterr()))
          return

      self.targets.run(self.list_command)
      for hn, t in self.targets.items():
        lines = t.lastout().split('\n')
        release = {}
        for line in lines:
          match = re.search('(.*) = (.*)', line)
          if match:
            name = match.group(1)
            version = match.group(2)
            try:
              release[name].append(version)
            except KeyError:
              release[name] = []
              release[name].append(version)

        for name in release:
          version = sorted(release[name], key=RPMVersion, reverse=True)[0]
          try:
            versions[hn].update({name:version})
          except KeyError:
            versions[hn] = {}
            versions[hn].update({name:version})

      for command in self.pre_commands:
        self.targets.run(command)

      for package in self.packages:
        temp = self.targets.copy()
        for hn in self.targets:
          try:
            command = self.install_command % (package, package, versions[hn][package])
            self.commands.update({hn:command})
          except KeyError:
            del temp[hn]

        temp.run(self.commands)

        for t in self.targets.values():
          self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())

      for command in self.post_commands:
        self.targets.run(command)

    except:
      raise
    finally:
      for t in self.targets.values():
        if not lock.locked:  # wasn't locked earlier by set_host_lock
          try:
            t.remove_lock()
          except AssertionError:
            pass

  def _check(self, target, stdin, stdout, stderr, exitcode):
    if 'A ZYpp transaction is already in progress.' in stderr:
      self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
      raise UpdateError(target.hostname, 'update stack locked')
    if 'System management is locked' in stderr:
      self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
      raise UpdateError('update stack locked', target.hostname)
    if '(c): c' in stdout:
      self.log.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
      raise UpdateError('Dependency Error', target.hostname)
    if exitcode == 104:
      self.log.critical('%s: zypper returned with errorcode 104:\n%s', target.hostname, stderr)
      raise UpdateError('Unspecified Error', target.hostname)

    return self.check(target, stdin, stdout, stderr, exitcode)

  def check(self, target, stdin, stdout, stderr, exitcode):
    """stub. needs to be overwritten by inherited classes"""
    return
