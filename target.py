#!/usr/bin/env python
# -*- coding: utf-8 -*-

import threading
import Queue
import re

from connection import *
from xmlout import *
from rpm import *

queue = Queue.Queue()

class Target():
	def __init__(self, hostname, system, packages=[]):
		self.hostname = hostname
		self.system = system
		self.log = []
		self.enabled = True

		try:
			self.connection = Connection(self.hostname)
		except:
			raise

		self.stdin = self.connection.stdin
		self.stdout = self.connection.stdout
		self.stderr = self.connection.stderr

		self.packages = Packages(packages)

	def query_versions(self, packages=None):
		versions = {}
		if packages == None:
			packages = self.packages.get_packages()

		if isinstance(packages, list):
			packages = " ".join(packages)

		self.connection.run("rpm -q %s" % packages)
		for line in re.split("\n+", self.connection.stdout()):
			match = re.search("(.*)-(.*)-(.*)", line)
			if match:
				versions[match.group(1)] = "%s-%s" % (match.group(2), match.group(3))
			else:
				match = re.search("package (.*) is not installed", line)
				if match:
					versions[match.group(1)] = "0"
					#print "warning: %s: package %s is not installed" % (self.hostname, match.group(1))

		return versions

	def query_version(self, package):
		versions = self.query_versions(package)

		return versions[package]

	def disable_repo(self, repo):
		self.connection.run("zypper mr -d %s" % repo)

	def enable_repo(self, repo):
		self.connection.run("zypper mr -e %s" % repo)

	def set_repo(self, name, state):
		repos = []
		self.connection.run("zypper lr")

		for match in re.finditer("^(\d+).*%s" % name, self.stdout(), re.M):
			repos.append(match.group(1))

		for repo in repos:
			if state == "disable":
				self.disable_repo(repo)
			elif state == "enable":
				self.enable_repo(repo)
			else:
				print "wrong state:", state

	def run(self, command):
		self.connection.run(command)
		self.log.append([command, self.stdout(), self.stderr()])

class Packages:
	def __init__(self, packages=[]):
		self.packages = packages
		self.versions = {}
		self.index = len(self.packages)

		for package in self.packages:
			self.versions[package] = {'before':0,'after':0,'required':0}

	def __iter__(self):
		return self

	def next(self):
		if self.index == 0:
			self.index = len(self.packages)
			raise StopIteration
		self.index = self.index - 1		
		return self.packages[self.index]

	def set_version(self, package, which, version):
		try:
			self.versions[package][which] = version
		except KeyError:
			print "package %s is not in list" % package

	def get_version(self, package, which):
		try:
			return self.versions[package][which]
		except KeyError:
			print "package %s is not in list" % package

	def set_versions(self, package, before=None, after=None, required=None):
		if before != None:
			self.set_version(package, "before", before)
		if after  != None:
			self.set_version(package, "after", after)
		if before != None:
			self.set_version(package, "required", required)

	def get_versions(self, package):
		return [self.versions[package][which] for which in ["before", "after", "required"]]

	def get_packages(self):
		return self.packages

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

			print "running method %s(%s)" % (method.__name__, parameter)

			try:
				method(*parameter)
			except:
				raise

			self.queue.task_done()

class FileUpload():
	def __init__(self, targets, local, remote):
		self.targets = targets
		self.local = local
		self.remote = remote

	def run(self):
		for target in self.targets:
			if self.targets[target].enabled:
				thread = ThreadedMethod(queue)
				thread.setDaemon(True)
				thread.start()

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].connection.put, [self.local, self.remote]])

		queue.join()

class FileDownload():
	def __init__(self, targets, remote, local, postfix=False):
		self.targets = targets
		self.remote = remote
		self.local = local
		self.postfix = postfix

	def run(self):
		for target in self.targets:
			if self.targets[target].enabled:
				thread = ThreadedMethod(queue)
				thread.setDaemon(True)
				thread.start()

		for target in self.targets:
			if self.targets[target].enabled:
				if self.postfix:
					queue.put([self.targets[target].connection.get, [self.remote, "%s.%s" % (self.local, target)]])
				else:
					queue.put([self.targets[target].connection.get, [self.remote, self.local]])

		queue.join()

class RunCommand():
	def __init__(self, targets, command):
		self.targets = targets
		self.command = command

	def run(self):
		for target in self.targets:
			if self.targets[target].enabled:
				thread = ThreadedMethod(queue)
				thread.setDaemon(True)
				thread.start()

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].run, [self.command]])

		queue.join()

class ZypperUpdate():
	def __init__(self, targets, patch):
		self.targets = targets
		self.patch = patch

	def run(self):
		commands = []
		for target in self.targets:
			if self.targets[target].enabled:
				thread = ThreadedMethod(queue)
				thread.setDaemon(True)
				thread.start()

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].set_repo, ["TESTING", "enable"]])

		queue.join()

		commands.append("export LANG=")
		commands.append("zypper lr -pu")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep \" %s \"" % self.patch)
		commands.append("for p in $(zypper patches | grep \" %s \" | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper install -n -l -y -t patch $p=%s; done" % (self.patch, self.patch))

		for command in commands: 
			for target in self.targets:
				if self.targets[target].enabled:
					queue.put([self.targets[target].run, [command]])

			queue.join()
			
class ZypperPrepare():
	def __init__(self, targets, packages):
		self.targets = targets

		if len(packages) > 1:
			self.packages = " ".join(packages)
		else:
			self.packages = packages

	def run(self):
		for target in self.targets:
			if self.targets[target].enabled:
				thread = ThreadedMethod(queue)
				thread.setDaemon(True)
				thread.start()

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].set_repo, ["TESTING", "disable"]])

		queue.join()

		command = "zypper -n in %s" % self.packages

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].run, [command]])

		for target in self.targets:
			if self.targets[target].enabled:
				queue.put([self.targets[target].set_repo, ["TESTING", "enable"]])
		
		queue.join()

class Metadata:
	def __init__(self):
		self.md5 = ""
		self.category = ""
		self.patches = {}
		self.swampid = ""
		self.packager = ""
		self.packages = {}
		self.systems = {}

	def get_package_list(self):
		return self.packages.keys()

