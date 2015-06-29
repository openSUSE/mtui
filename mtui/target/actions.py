# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function

import threading
try:
  from queue import Queue
except ImportError:
  from Queue import Queue
import sys
import time

from mtui.utils import prompt_user


queue = Queue()


class ThreadedMethod(threading.Thread):
  def __init__(self, queue):
    threading.Thread.__init__(self)
    self.queue = queue

  def run(self):
    while True:
      try:
        (method, parameter) = self.queue.get(timeout=10)
      except:
        return

      try:
        method(*parameter)
      except:
        raise
      finally:
        try:
          self.queue.task_done()
        except ValueError:
          pass  # already removed by ctrl+c


class ThreadedTargetGroup(object):
  def __init__(self, targets):
    self.targets = targets

  def mk_thread(self):
    thread = ThreadedMethod(queue)
    thread.setDaemon(True)
    thread.start()

  def mk_threads(self):
    for _ in range(0, len(self.targets)):
      self.mk_thread()

  def run(self):
    self.mk_threads()
    self.setup_queue()

    while queue.unfinished_tasks:
      spinner()

    queue.join()

  def setup_queue(self):
    for t in self.targets:
      queue.put(self.mk_cmd(t))


class FileDelete(ThreadedTargetGroup):
  def __init__(self, targets, path):
    super(FileDelete, self).__init__(targets)
    self.path = path

  def mk_cmd(self, t):
    return [t.remove, [self.path]]


class FileUpload(ThreadedTargetGroup):
  def __init__(self, targets, local, remote):
    super(FileUpload, self).__init__(targets)
    self.local = local
    self.remote = remote

  def mk_cmd(self, t):
    return [t.put, [self.local, self.remote]]


class FileDownload(ThreadedTargetGroup):
  def __init__(self, targets, remote, local, postfix=False):
    super(FileDownload, self).__init__(targets)

    self.remote = remote
    self.local = local
    self.postfix = postfix

  def local_name(self, t):
    """
    :type t: L{Target} instance
    """
    if not self.postfix:
      return self.local

    return '{0}.{1}'.format(self.local, t.hostname)

  def mk_cmd(self, t):
    return [t.get, [self.remote, self.local_name(t)]]


class RunCommand(object):
  def __init__(self, targets, command):
    self.targets = targets
    self.command = command

  def run(self):
    parallel = {}
    serial = {}
    lock = threading.Lock()

    for target in self.targets:
      if self.targets[target].exclusive:
        serial[target] = self.targets[target]
      else:
        parallel[target] = self.targets[target]

    try:
      for target in parallel:
        thread = ThreadedMethod(queue)
        thread.setDaemon(True)
        thread.start()
        if type(self.command) == dict:
          queue.put([parallel[target].run, [self.command[target], lock]])
        elif type(self.command) == str:
          queue.put([parallel[target].run, [self.command, lock]])

      while queue.unfinished_tasks:
        spinner(lock)

      queue.join()

      for target in serial:
        prompt_user('press Enter key to proceed with %s' % serial[target].hostname, '')
        thread = ThreadedMethod(queue)
        thread.setDaemon(True)
        thread.start()
        queue.put([serial[target].run, [self.command, lock]])
        while queue.unfinished_tasks:
          spinner(lock)

        queue.join()
    except KeyboardInterrupt:
      print('stopping command queue, please wait.')
      try:
        while queue.unfinished_tasks:
          spinner(lock)
      except KeyboardInterrupt:
        for target in self.targets:
          try:
            self.targets[target].connection.close_session()
          except Exception:
            pass
        try:
          thread.queue.task_done()
        except ValueError:
          pass

      queue.join()
      print()
      raise


def spinner(lock=None):
  """simple spinner to show some process"""

  for pos in ['|', '/', '-', '\\']:
    if lock is not None:
      lock.acquire()

    try:
      sys.stdout.write('processing... [%s]\r' % pos)
      sys.stdout.flush()
    finally:
      if lock is not None:
        lock.release()

    time.sleep(0.3)
