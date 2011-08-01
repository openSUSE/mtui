#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import re
import threading
import Queue
import logging

from connection import *
from xmlout import *
from utils import *

out = logging.getLogger('mtui')

queue = Queue.Queue()

class Target():
	def __init__(self, hostname, system, packages=[], dryrun=False, timeout=None):
		self.hostname = hostname
		self.system = system
		self.packages = {}
		self.log = []

		if dryrun:
			self.state = "dryrun"
		else:
			self.state = "enabled"

		if timeout is None:
			timeout = 300

		out.info("connecting to %s" % self.hostname)

		try:
			self.connection = Connection(self.hostname, timeout)
		except Exception as error:
			out.critical("connecting to %s failed: %s" % (self.hostname, str(error)))
			raise

		for package in packages:
			self.packages[package] = Package(package)

	def query_versions(self, packages=None):
		versions = {}
		if packages is None:
			packages = self.packages.keys()

		if isinstance(packages, list):
			packages = " ".join(packages)

		if self.state == "enabled":
			self.run("rpm -q %s" % packages)

			for line in re.split("\n+", self.lastout()):
				match = re.search(r"^([a-zA-Z0-9_\-\+]*)-([a-zA-Z0-9_\.]*)-([a-zA-Z0-9_\.]*)", line)
				if match:
					self.packages[match.group(1)].current = "%s-%s" % (match.group(2), match.group(3))
				else:
					match = re.search("package (.*) is not installed", line)
					if match:
						self.packages[match.group(1)].current = "0"
						out.debug("%s: package %s is not installed" % (self.hostname, match.group(1)))

		elif self.state == "dryrun":
			out.info('dryrun: %s running "rpm -q %s"' % (self.hostname, packages))
			self.log.append(["rpm -q %s" % packages, "dryrun\n", "", 0, 0])

		elif self.state == "disabled":
			self.log.append(["", "", "", 0, 0])

	def query_version(self, package):
		self.query_versions(package)
		return self.packages[package].current

	def disable_repo(self, repo):
		out.debug("disabling repo %s on %s" % (repo, self.hostname))
		self.run("zypper mr -d %s" % repo)

	def enable_repo(self, repo):
		out.debug("enabling repo %s on %s" % (repo, self.hostname))
		self.run("zypper mr -e %s" % repo)

	def set_timeout(self, value):
		out.debug("setting timeout to %s on %s" % (value, self.hostname))
		self.connection.timeout = value

	def get_timeout(self):
		return self.connection.timeout

	def set_repo(self, name):
		command = "/suse/rd-qa/bin/rep-clean.sh"

		try:
			repclean = self.connection.open(command, "r")
		except IOError:
			home = os.path.dirname(__file__)
			try:
				self.put("%s/helper/rep-clean/rep-clean.sh" % home, "/tmp/rep-clean.sh")
				self.put("%s/helper/rep-clean/rep-clean.conf" % home, "/tmp/rep-clean.conf")
			except OSError:
				out.error("missing rep-clean.sh script")

			command = "/tmp/rep-clean.sh -F /tmp/rep-clean.conf"
		else:
			repclean.close()

		if name == 'TESTING':
			out.debug("enabling TESTING repos on %s" % self.hostname)
			parameter = '-t'
		elif name == "UPDATE":
			out.debug("enabling UPDATE repos on %s" % self.hostname)
			parameter = '-n'

		self.run("%s %s" % (command, parameter))

	def run(self, command, lock=None):
		if self.state == "enabled":
			out.debug('%s: running "%s"' % (self.hostname, command))
			try:
				time_before = timestamp()
				exitcode = self.connection.run(command, lock)
				time_after = timestamp()
			except CommandTimeout:
				out.critical('%s: command "%s" timed out' % (self.hostname, command))
				exitcode = -1

			runtime = int(time_after) - int(time_before)
			self.log.append([command, self.connection.stdout, self.connection.stderr, exitcode, runtime])

		elif self.state == "dryrun":
			out.info('dryrun: %s running "%s"' % (self.hostname, command))
			self.log.append([command, "dryrun\n", "", 0, 0])

		elif self.state == "disabled":
			self.log.append(["", "", "", 0, 0])

	def put(self, local, remote):
		if self.state == "enabled":
			return self.connection.put(local, remote)
		elif self.state == "dryrun":
			out.info('dryrun: put %s %s:%s' % (local, self.hostname, remote))
		

	def get(self, remote, local):
		if self.state == "enabled":
			return self.connection.get(remote, local)
		elif self.state == "dryrun":
			out.info('dryrun: get %s:%s %s' % (self.hostname, remote, local))

	def lastin(self):
		try:
			return self.log[-1][0]
		except:
			return ""

	def lastout(self):
		try:
			return self.log[-1][1]
		except:
			return ""

	def lasterr(self):
		try:
			return self.log[-1][2]
		except:
			return ""

	def lastexit(self):
		try:
			return self.log[-1][3]
		except:
			return ""

	def lastruntime(self):
		try:
			return self.log[-1][4]
		except:
			return ""

	def locked(self):
		lock = Locked(False)

		if self.state == "enabled":
			try:
				lockfile = self.connection.open("/var/lock/mtui.lock")

			except IOError as error:
				if error.errno == errno.ENOENT:
					return lock

			line = lockfile.readline().strip()
			try:
				lock.timestamp, lock.user, lock.pid = line.split(':')
			except Exception:
				return lock

			lock.locked = True

			lockfile.close()

		return lock

	def set_locked(self):
		if self.state == "enabled":
			try:
				lockfile = self.connection.open("/var/lock/mtui.lock", "w+")

			except IOError as error:
				out.error(str(error))
				return

			now = timestamp()
			user = os.getlogin()
			pid = os.getpid()
			lockfile.write("%s:%s:%s" % (now, user, pid))
			lockfile.close()

	def remove_lock(self):
		if self.state == "enabled":
			lock = self.locked()

			if lock.locked:
				assert lock.own()
				try:
					self.connection.remove("/var/lock/mtui.lock")
				except IOError as error:
					if error.errno == errno.ENOENT:
						out.debug("lockfile does not exist")

	def close(self):
		out.info("closing connection to %s" % self.hostname)
		return self.connection.close()

class Package:
	def __init__(self, name):
		self.name = name
		self.before = "0"
		self.after = "0"
		self.required = "0"
		self.current = "0"

	def set_versions(self, before=None, after=None, required=None, current=None, versions=[]):
		if before is not None:
			self.before = before
		if after is not None:
			self.after = after
		if required is not None:
			self.required = required
		if current is not None:
			self.current = current
		if versions:
			self.before = versions[0]
			self.after = versions[1]
			self.required = versions[2]

	def get_versions(self):
		return [ self.before, self.after, self.required ]

class Metadata:
	def __init__(self):
		self.md5 = ""
		self.path = ""
		self.category = ""
		self.patches = {}
		self.swampid = ""
		self.packager = ""
		self.packages = {}
		self.systems = {}
		self.bugs = {}

	def get_package_list(self):
		return self.packages.keys()

	def get_releases(self):
		releases = []
		systems = " ".join(self.systems.values())
		if re.search("sle.11", systems):
			releases.append("11")
		if re.search("sle.10", systems):
			releases.append("10")
		if re.search("sle.9", systems):
			releases.append("9")

		return releases

class ThreadedMethod(threading.Thread):
	def __init__(self, queue):
		threading.Thread.__init__(self)
		self.queue = queue

	def run(self):
		while True:
			try:
				method, parameter = self.queue.get(timeout=10)
			except:
				return

			out.debug("running method %s(%s)" % (method.__name__, parameter))

			try:
				method(*parameter)
			except:
				raise
			finally:
				self.queue.task_done()


class FileUpload():
	def __init__(self, targets, local, remote):
		self.targets = targets
		self.local = local
		self.remote = remote

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			try:
				queue.put([self.targets[target].put, [self.local, self.remote]])
			except:
				raise

		while queue.unfinished_tasks:
			spinner()

		queue.join()

class FileDownload():
	def __init__(self, targets, remote, local, postfix=False):
		self.targets = targets
		self.remote = remote
		self.local = local
		self.postfix = postfix

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			if self.postfix:
				queue.put([self.targets[target].get, [self.remote, "%s.%s" % (self.local, target)]])
			else:
				queue.put([self.targets[target].get, [self.remote, self.local]])

		while queue.unfinished_tasks:
			spinner()

		queue.join()

class RunCommand():
	def __init__(self, targets, command):
		self.targets = targets
		self.command = command

	def run(self):
		lock = threading.Lock()

		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		try:
			for target in self.targets:
				queue.put([self.targets[target].run, [self.command, lock]])

			while queue.unfinished_tasks:
				spinner(lock)

			queue.join()

		except KeyboardInterrupt:
			print "stopping command queue, please wait"
			queue.join()
			raise

class Locked():
	def __init__(self, locked=False, user="nobody", timestamp=0, pid=0):
		self.locked = locked
		self.user = user
		self.timestamp = timestamp
		self.pid = pid

	def own(self):
		if self.user == os.getlogin() and self.pid == str(os.getpid()):
			return True
		else:
			return False

	def time(self, style=None):
		from datetime import datetime

		if style is None:
			style = "%A, %d.%m.%Y %H:%M"

		time = datetime.fromtimestamp(float(self.timestamp))

		return time.strftime(style)

def spinner(lock=None):
	"""simple spinner to show some process"""

	for pos in ['|', '/', '-', '\\']:
		if lock is not None:
			lock.acquire()

		try:
			sys.stdout.write("processing... [%s]\r" % pos)
			sys.stdout.flush()
		finally:
			if lock is not None:
				lock.release()

		time.sleep(0.3)

