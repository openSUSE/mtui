# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from os.path import basename
from os.path import splitext

from traceback import format_exc

import subprocess

from mtui import messages

class Script(object):
  """
  :type subdir: str
  :param subdir: subdirectory in the L{TestReport.scripts_wd} where the
    scripts are located.

    Note: also used as a "type of the script" and can be shown to
    the user.

  FIXME: should be an abstract attribute
  """

  def __init__(self, tr, path, log):
    """
    :type path: str
    :param path: absolute path to the script
    """
    self.path = path
    self.name = basename(path)
    self.bname = splitext(self.name)[0]
    self.testreport = tr
    self.log = log

  def __repr__(self):
    return "<{0}.{1} {2} for {3}>".format(
      self.__module__,
      self.__class__.__name__,
      self.path,
      repr(self.testreport),
    )

  def __str__(self):
    return "{0} script {1}".format(
      self.subdir,
      self.name,
    )

  def _result(self, T, bname, t):
    return self.testreport.report_wd(*T.result_parts(bname, t.hostname), filepath = True)

  @classmethod
  def result_parts(T, *basename):
    return ('output/scripts', '.'.join((T.subdir,) + basename))

  def run(self, targets):
    """
    :type targets: [{HostsGroup}]
    """
    try:
      self.log.info('running {0}'.format(self))
      self._run(targets)
    except KeyboardInterrupt:
      self.log.warning('skipping {0}'.format(self))
      return

class PreScript(Script):
  subdir = "pre"

  def _run(self, targets):
    rname = self.testreport.target_wd("%s.%s" % (self.subdir, self.bname))
    targets.put(
      self.path,
      rname,
    )

    targets.put(
      self.testreport.pkg_list_file(),
      self.testreport.target_wd('package-list.txt'),
    )

    targets.run(
      "{exe} -r {repository} -p {pkg_list_file} {id}".format(
        exe = rname,
        repository = self.testreport.repository,
        pkg_list_file = self.testreport.target_wd('package-list.txt'),
        id  = self.testreport.id,
      )
    )

    for t in targets.values():
      fname = self._result(type(self), self.bname, t)
      try:
        with open(fname, 'w') as f:
          f.write(t.lastout())
          f.write(t.lasterr())
      except IOError as e:
        self.log.error(messages.FailedToWriteScriptResult(fname, e))

class PostScript(PreScript):
  subdir = "post"

class CompareScript(Script):
  subdir = "compare"

  def _run(self, targets):
    for t in targets.values():
      self._run_single_target(t)

  def _run_single_target(self, t):
    bcheck = self.bname.replace("compare_", "check_")
    argv = [
      self.path,
      self._result(PreScript, bcheck, t),
      self._result(PostScript, bcheck, t),
    ]

    self.log.debug("running {0}".format(argv))
    stdout = stderr = None
    try:
      p = subprocess.Popen(
        argv,
        stdout = subprocess.PIPE,
        stderr = subprocess.PIPE,
      )
    except EnvironmentError as e:
      t.log.append([' '.join(argv), '', '', 0x100, 0])
      self.log.critical(messages.StartingCompareScriptError(e, argv))
      self.log.debug(format_exc())
      return

    (stdout, stderr) = p.communicate()
    rc = p.wait()

    t.log.append([' '.join(argv), str(stdout), str(stderr), rc, 0])

    if rc == 0:
      return

    if rc == 2:
      logger, msg = self.log.critical, messages.CompareScriptCrashed
    else:
      logger, msg = self.log.warning, messages.CompareScriptFailed

    assert callable(logger), "{0!r} not callable".format(logger)

    logger(msg(argv, stdout, stderr, rc))

