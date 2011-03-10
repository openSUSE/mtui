#!/usr/bin/env python
# -*- coding: utf-8 -*-

import time
import sys

from target import *

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

class ZypperUpdate(Update):
	def __init__(self, targets, patches):
		Update.__init__(self, targets, patches)

		patch = patches['sat']

		commands = []

		commands.append("export LANG=")
		commands.append("zypper lr -pu")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep \" %s \"" % patch)
		commands.append("for p in $(zypper patches | grep \" %s \" | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper -n install -l -y -t patch $p=%s; done" % (patch, patch))
			
		self.commands = commands

class RugUpdate(Update):
	def __init__(self, targets, patch):
		Update.__init__(self, targets, patch)

		patch = patches['zypp']

		commands = []

		commands.append("export LANG=")
		commands.append("zypper sl")
		commands.append("zypper refresh")
		commands.append("zypper patches | grep \" %s \"" % patch)
		commands.append("for p in $(zypper patches | grep \" %s \" | awk 'BEGIN { FS=\"|\"; } { print $2; }'); do zypper -n install -l -y -t patch $p; done" % patch)
			
		self.commands = commands

class OnlineUpdate(Update):
	def __init__(self, targets, patch):
		Update.__init__(self, targets, patch)

		commands = []

		commands.append("export LANG=")

		self.commands = commands

Updater = {
    '11': ZypperUpdate,
    '10': RugUpdate,
    '9': OnlineUpdate
}

