#!/usr/bin/env python
# -*- coding: utf-8 -*-

import time
import sys

from target import *

class UpdateError(Exception):
	def __init__(self, type, where):
		self.type = type
		self.where = where

		self.string = "%s failed on %" % (self.type, self.where)

	def __str__(self):
		return repr(self.string)

class Update():
	def __init__(self, targets, patches):
		self.targets = targets
		self.patches = patches

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["TESTING", "enable"]])

		while queue.qsize():
			spinner()

		queue.join()

		for command in self.commands: 
			for target in self.targets:
				queue.put([self.targets[target].run, [command]])

			while queue.qsize():
				spinner()

			queue.join()

			for target in self.targets:
				self.check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(), self.targets[target].lasterr(), self.targets[target].lastexit())

	def check(self, target, stdin, stdout, stderr, exitcode):
		"""stub. needs to be overwritten by inherited classes"""

		return

class ZypperUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['sat']

		commands = []

		commands.append("export LANG=")
		commands.append("zypper lr -pu")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep \" %s \"" % patch)
		commands.append("for p in $(zypper patches | grep \" %s \" | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper install -l -y -t patch $p=%s; done" % (patch, patch))
			
		self.commands = commands

	def check(self, target, stdin, stdout, stderr, exitcode):
		if "zypper" in stdin and exitcode == "104":
			out.error("%s: command %s failed: stdin:\n%sstderr:\n%s", target.hostname, stdin, stdout, stderr)

class OldZypperUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['zypp']

		commands = []

		commands.append("export LANG=")
		commands.append("zypper sl")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep %s-0" % patch)
		commands.append("for p in $(zypper patches | grep  %s-0 | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper in -y -t patch $p; done" % patch)
			
		self.commands = commands

class OnlineUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['you']

		commands = []

		commands.append("export LANG=")
		commands.append("find /var/lib/YaST2/you/ -name patch-%s" % patch)
		commands.append("online_update -V --url http://you.suse.de/download -S patch-%s -f" % patch)
		commands.append("find /var/lib/YaST2/you/ -name patch-%s" % patch)

		self.commands = commands

class RugUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['you']

		commands = []

		commands.append("export LANG=")
		commands.append("rug sl")
		commands.append("rug refresh")
		commands.append("rug patch-info patch-%s" % patch)
		commands.append("rug patch-install patch-%s" % patch)

		self.commands = commands

Updater = {
    '11': ZypperUpdate,
    '10': OldZypperUpdate,
    '9': OnlineUpdate,
    'OES': RugUpdate
}

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

		while queue.qsize():
			spinner()

		queue.join()

		for target in self.targets:
			if self.targets[target].lastexit() != 0:
				out.error("failed to disable TESTING repos on %s: zypper exit code %s" % (target, self.targets[target].lastexit()))
				raise UpdateError("disable repo", target)

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["UPDATE", "enable"]])

		while queue.qsize():
			spinner()

		queue.join()

		for target in self.targets:
			if self.targets[target].lastexit() != 0:
				out.error("failed to enable UPDATE repos on %s: zypper exit code %s" % (target, self.targets[target].lastexit()))
				raise UpdateError("disable repo", target)

		for package in self.packages:
			command = "zypper -n in --no-recommends -y -l %s" % package
			for target in self.targets:
				queue.put([self.targets[target].run, [command]])

		while queue.qsize():
			spinner()

		queue.join()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["UPDATE", "disable"]])
			queue.put([self.targets[target].set_repo, ["TESTING", "enable"]])

		while queue.qsize():
			spinner()
		
		queue.join()

Preparer = {
    '11': ZypperPrepare
}
