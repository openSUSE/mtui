# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function

from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue
from mtui.target.actions import spinner

from mtui.target.locks import LockedTargets

from mtui.hooks import PreScript
from mtui.hooks import PostScript
from mtui.hooks import CompareScript

from mtui.rpmver import RPMVersion
from mtui.utils import yellow


class Update(object):
  def __init__(self, logger, targets, patches, packages, testreport):
    self.log = logger
    self.targets = targets
    self.patches = patches
    self.packages = packages
    self.testreport = testreport
    self.commands = []

  def run(self, params):
    with LockedTargets(self.targets.values()):
      self._run(params)

  def _run(self, params):
    if 'noprepare' not in params:
      self.testreport.perform_prepare(self.targets)

    for hn, t in self.targets.items():
      not_installed = []

      t.query_versions()

      for pkgname, pkg in t.packages.items():
        required = self.testreport.packages[pkgname]
        before = pkg.current

        pkg.set_versions(before = before, required = required)

        if before is None or before == '0':
          not_installed.append(pkgname)
        else:
          if RPMVersion(before) >= RPMVersion(required):
            self.log.warning('%s: package is too recent: %s (%s, target version is %s)' % (hn, pkgname, before, required))

      if not_installed:
        self.log.warning('%s: these packages are missing: %s' % (hn, not_installed))

    if 'noscript' not in params:
      self.testreport.run_scripts(PreScript, self.targets)

    self.log.info('updating')

    skipped = False

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
        queue.put([t.set_repo, ['TESTING', self.testreport]])

      while queue.unfinished_tasks:
        spinner()

      queue.join()

      for command in self.commands:
        self.targets.run(command)

        for t in self.targets.values():
          self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())
    except:
      raise
    finally:
      for t in self.targets.values():
        if not lock.locked:  # wasn't locked earlier by set_host_lock
          try:
            t.remove_lock()
          except AssertionError:
            pass

    if 'newpackage' in params:
      self.testreport.perform_prepare(self.targets, testing = True)

    for hn, t in self.targets.items():
      t.query_versions()

      for pkgname, pkg in t.packages.items():
        before = pkg.before
        required = pkg.required
        after = pkg.current

        pkg.set_versions(after=after)

        if after is not None and after != '0':
          if RPMVersion(before) == RPMVersion(after):
            self.log.warning('%s: package was not updated: %s (%s)' % (hn, pkgname, after))

          if RPMVersion(after) < RPMVersion(required):
            self.log.warning('%s: package does not match required version: %s (%s, required %s)' % (hn, pkgname, after, required))

    if 'noscript' not in params:
      self.testreport.run_scripts(PostScript, self.targets)
      self.testreport.run_scripts(CompareScript, self.targets)
      self.targets.remove(self.testreport.target_wd('output'))


  def _check(self, target, stdin, stdout, stderr, exitcode):
    if 'zypper' in stdin and exitcode == 104:
      self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
      raise UpdateError('update stack locked', target.hostname)
    if 'Additional rpm output' in stdout:
      self.log.warning('There was additional rpm output on %s:', target.hostname)
      marker = 'Additional rpm output:'
      start = stdout.find(marker) + len(marker)
      end = stdout.find('Retrieving', start)
      print(stdout[start:end].replace('warning', yellow('warning')))
    if 'A ZYpp transaction is already in progress.' in stderr:
      self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
      raise UpdateError('update stack locked', target.hostname)
    if 'System management is locked' in stderr:
      self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
      raise UpdateError('update stack locked', target.hostname)
    if '(c): c' in stdout:
      self.log.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
      raise UpdateError('Dependency Error', target.hostname)

    return self.check(target, stdin, stdout, stderr, exitcode)

  def check(self, target, stdin, stdout, stderr, exitcode):
    """stub. needs to be overwritten by inherited classes"""
    return
