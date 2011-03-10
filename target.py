#!/usr/bin/env python
# -*- coding: utf-8 -*-

import threading
import Queue
import re
import logging

from connection import *
from xmlout import *

out = logging.getLogger('mtui')

queue = Queue.Queue()

class Target():
	def __init__(self, hostname, system, packages=[], dryrun=False):
		self.hostname = hostname
		self.system = system
		self.log = []
		self.packages = {}

		if dryrun:
			self.state = "dryrun"
		else:
			self.state = "enabled"

		try:
			self.connection = Connection(self.hostname)
		except:
			raise

		self.stdin = self.connection.stdin
		self.stdout = self.connection.stdout
		self.stderr = self.connection.stderr

		try:
			for package in packages:
				self.packages[package] = Package(package)
		except Exception as error:
			print error

	def query_versions(self, packages=None):
		versions = {}
		if packages == None:
			packages = self.packages.keys()

		if isinstance(packages, list):
			packages = " ".join(packages)

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

	def query_version(self, package):
		self.query_versions(package)
		return self.packages[package].current

	def disable_repo(self, repo):
		out.debug("disabling repo %s on %s" % (repo, self.hostname))
		self.run("zypper mr -d %s" % repo)

	def enable_repo(self, repo):
		out.debug("enabling repo %s on %s" % (repo, self.hostname))
		self.run("zypper mr -e %s" % repo)

	def set_repo(self, name, state):
		repos = []
		self.run("zypper lr")

		for match in re.finditer("^(\d+).*%s" % name, self.lastout(), re.M):
			repos.append(match.group(1))

		for repo in repos:
			if state == "disable":
				self.disable_repo(repo)
				out.debug("disabled repo %s on %s" % (repo, self.hostname))
			elif state == "enable":
				self.enable_repo(repo)
				out.debug("enabled repo %s on %s" % (repo, self.hostname))
			else:
				out.error("setting repository to %s failed. wrong state." % state)

	def run(self, command):
		if self.state == "enabled":
			out.debug('running "%s" on %s' % (command, self.hostname))
			exitcode = self.connection.run(command)
			self.log.append([command, self.stdout(), self.stderr(), exitcode])
		elif self.state == "dryrun":
			out.info('dryrun: running "%s" on %s' % (command, self.hostname))
			self.log.append([command, "dryrun\n", "", 0])

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
		return self.log[-1][0]

	def lastout(self):
		return self.log[-1][1]

	def lasterr(self):
		return self.log[-1][2]

	def lastexit(self):
		return self.log[-1][3]

	def close(self):
		return self.connection.close()

class Package:
	def __init__(self, name):
		self.name = name
		self.before = "0"
		self.after = "0"
		self.required = "0"
		self.current = "0"

	def set_versions(self, before=None, after=None, required=None, current=None, versions=[]):
		if before != None:
			self.before = before
		if after  != None:
			self.after = after
		if required != None:
			self.required = required
		if current != None:
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
		self.category = ""
		self.patches = {}
		self.swampid = ""
		self.packager = ""
		self.packages = {}
		self.systems = {}
		self.bugs = {}

	def get_package_list(self):
		return self.packages.keys()

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

		while queue.qsize():
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

		while queue.qsize():
			spinner()

		queue.join()

class RunCommand():
	def __init__(self, targets, command):
		self.targets = targets
		self.command = command

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			queue.put([self.targets[target].run, [self.command]])

		while queue.qsize():
			spinner()

		queue.join()

class ZypperPrepare():
	def __init__(self, targets, packages):
		self.targets = targets
		self.packages = packages

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["TESTING", "disable"]])
			queue.put([self.targets[target].set_repo, ["UPDATE", "enable"]])

		while queue.qsize():
			spinner()

		queue.join()

		for package in self.packages:
			command = "zypper -n in --no-recommends -y -l %s" % package
			for target in self.targets:
				queue.put([self.targets[target].run, [command]])

		while queue.qsize():
			spinner()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["UPDATE", "disable"]])
			queue.put([self.targets[target].set_repo, ["TESTING", "enable"]])

		while queue.qsize():
			spinner()
		
		queue.join()

def spinner():
	"""simple spinner to show some process"""

	import time
	import sys

	for pos in ['|', '/', '-', '\\']:
		sys.stdout.write("processing... [%s]\r" % pos)
		sys.stdout.flush()
		time.sleep(0.3)

