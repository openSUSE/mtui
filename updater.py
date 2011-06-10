#!/usr/bin/env python
# -*- coding: utf-8 -*-

import time
import sys

from target import *

class UpdateError(Exception):
	def __init__(self, host, reason):
		self.host = host
		self.reason = reason

	def __str__(self):
		return repr("%s: %s" % (self.host, self.reason))

class Update():
	def __init__(self, targets, patches):
		self.targets = targets
		self.patches = patches
		self.commands = []

	def run(self):
		lock = threading.Lock()

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
			RunCommand(self.targets, command).run()

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
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "update stack locked")

class OldZypperUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['zypp']

		commands = []

		commands.append("export LANG=")
		commands.append("zypper sl")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep %s-0" % patch)
		commands.append("for p in $(zypper patches | grep %s-0 | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper in -l -y -t patch $p; done" % patch)
			
		self.commands = commands

	def check(self, target, stdin, stdout, stderr, exitcode):
		if "A ZYpp transaction is already in progress." in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "update stack locked")
		if "Error:" in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "RPM Error")

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
	def __init__(self, targets, packages=None, patches=None):
		self.targets = targets
		self.packages = packages
		self.patches = patches
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
				out.critical("could not prepare host %s. stopping.\n# %s\n%s" % (target, self.targets[target].lastin(), self.targets[target].lasterr()))
				return

		for command in self.commands: 
			RunCommand(self.targets, command).run()

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

	def check(self, target, stdin, stdout, stderr, exitcode):
		if "A ZYpp transaction is already in progress." in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "update stack locked")
		if "Error:" in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "RPM Error")

Preparer = {
    '11': ZypperPrepare,
    '10': OldZypperPrepare
}

class ZypperDowngrade(Prepare):
	def __init__(self, targets, packages, patches):
		Prepare.__init__(self, targets, packages, patches)

		commands = []

		for package in packages:
			commands.append("zypper -n in --force-resolution -y -l %s=$(zypper se -s --match-exact %s | grep -v \"(System Packages)\" | grep ^[iv] | cut -d \| -f 4 | sort -ru | head -n 1 | sed -e 's, ,,g')" % (package, package))

		self.commands = commands

class OldZypperDowngrade(Prepare):
	def __init__(self, targets, packages, patches):
		Prepare.__init__(self, targets, packages, patches)

		patch = patches['zypp']

		invalid_packages = ['glibc', 'rpm', 'zypper']
		invalid = set(packages).intersection(invalid_packages)
		if invalid:
			out.critical("crucial package found in package list: %s. please downgrade manually" % list(invalid))
			return

		commands = []

		commands.append("for p in $(zypper patches | grep %s-0 | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper rm -y -t patch $p; done" % patch)

		for package in packages:
			commands.append("rpm --nodeps -e %s" % package)

		commands.append("for p in $(zypper patches | grep %s-0 | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper rm -y -t patch $p; done" % patch)

		for package in packages:
			commands.append("zypper rm -y -t atom %s" % package)

		for package in packages:
			commands.append("zypper -n in -y -l %s" % package)

		self.commands = commands

	def check(self, target, stdin, stdout, stderr, exitcode):
		if "A ZYpp transaction is already in progress." in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "update stack locked")
		if "Error:" in stderr:
			out.critical('%s: command "%s" failed:\nstdin:\n%sstderr:\n%s', target.hostname, stdin, stdout, stderr)
			raise UpdateError(target.hostname, "RPM Error")

Downgrader = {
    '11': ZypperDowngrade,
    '10': OldZypperDowngrade
}

