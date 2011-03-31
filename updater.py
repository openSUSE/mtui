#!/usr/bin/env python
# -*- coding: utf-8 -*-

import time
import sys

from target import *

class UpdateError(Exception):
	def __init__(self, value):
		self.value = value

	def __str__(self):
		return repr(self.value)

class Update():
	def __init__(self, targets, patches):
		self.targets = targets
		self.patches = patches
		self.commands = []

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["TESTING"]])

		while queue.unfinished_tasks:
			spinner()

		queue.join()

		for command in self.commands: 
			for target in self.targets:
				queue.put([self.targets[target].run, [command]])

			while queue.unfinished_tasks:
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
			out.error("%s: command %s failed:\nstdin:\n%sstderr:\n%s", target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname)

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

	def check(self, target, stdin, stdout, stderr, exitcode):
		if "A ZYpp transaction is already in progress." in stderr:
			out.error("%s: command %s failed:\nstdin:\n%sstderr:\n%s", target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname)

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

class Prepare():
	def __init__(self, targets, packages):
		self.targets = targets
		self.packages = packages
		self.commands = []

	def run(self):
		for target in self.targets:
			thread = ThreadedMethod(queue)
			thread.setDaemon(True)
			thread.start()

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["UPDATE"]])

		while queue.unfinished_tasks:
			spinner()

		queue.join()

		for target in self.targets:
			if self.targets[target].lasterr():
				out.error("could not prepare host %s:\n# %s\n%s" % (target, self.targets[target].lastin(), self.targets[target].lasterr()))
				return

		for command in self.commands: 
			for target in self.targets:
				queue.put([self.targets[target].run, [command]])

			while queue.unfinished_tasks:
				spinner()

			queue.join()

			for target in self.targets:
				self.check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(), self.targets[target].lasterr(), self.targets[target].lastexit())

		for target in self.targets:
			queue.put([self.targets[target].set_repo, ["TESTING"]])

		while queue.unfinished_tasks:
			spinner()
		
		queue.join()

	def check(self, target, stdin, stdout, stderr, exitcode):
		"""stub. needs to be overwritten by inherited classes"""

		return

class ZypperPrepare(Prepare):
	def __init__(self, targets, packages):
		Prepare.__init__(self, targets, packages)

		commands = []

		for package in packages:
			commands.append("zypper -n in --no-recommends -y -l %s" % package)

		self.commands = commands

class OldZypperPrepare(Prepare):
	def __init__(self, targets, packages):
		Prepare.__init__(self, targets, packages)

		commands = []

		for package in packages:
			commands.append("zypper -n in -y -l %s" % package)

		self.commands = commands

Preparer = {
    '11': ZypperPrepare,
    '10': OldZypperPrepare
}

class ZypperDowngrade(Prepare):
	def __init__(self, targets, packages):
		Prepare.__init__(self, targets, packages)

		commands = []

		for package in packages:
			commands.append("zypper -n in --force-resolution -y -l %s=$(zypper se -s --match-exact %s | grep ^v | cut -d \| -f 4 | sort -ru | head -n 1 | sed -e 's, ,,g')" % (package, package))

		self.commands = commands


Downgrader = {
    '11': ZypperDowngrade
}

