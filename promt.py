#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import stat
import cmd

from rpm import *

class CommandPromt(cmd.Cmd):
 
	prompt = 'QA > '
 
	def __init__(self, targets, metadata):
		cmd.Cmd.__init__(self)
		self.targets = targets
		self.metadata = metadata
 
	def do_add_host(self, args):
		"""connect to another host and add it to the list

		add_host <hostname,system>
		Keyword arguments:
		hostname -- hostname or address of the host
		system   -- system type, ie. sles11sp1-i386

		"""
		if args:
			try:
				hostname, system = args.split(',')
			except ValueError:
				parse_error(self.do_add_host, args)
				return

			self.targets[hostname] = Target(hostname, system, self.metadata.get_package_list())
		else:
			parse_error(self.do_add_host, args)
 
	def do_delete_host(self, args):
		"""disconnect from host and remove host from list

		delete_host <hostname>,hostname,...
		Keyword arguments:
		hostname -- hostname or address of the host

		"""
		if args:
			for target in args.split(','):
				try:
					self.targets[target].connection.close()
					del self.targets[target]
				except KeyError:
					print "host %s not in database" % target

		else:
			parse_error(self.do_delete_host, args)
 
 	def do_list_hosts(self, args):
		"""list connected hosts and current status

		list_hosts
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_list_hosts, args)

		else:
			for target in self.targets:
				if self.targets[target].enabled:
					state = green('Enabled')
				else:
					state = red('Disabled')

				print "%s\t:\t%s" % (target, state)

 	def do_list_scripts(self, args):
		"""list available scripts from the scripts subdirectory

		list_scripts 
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_list_scripts, args)

		else:
			try:
				for root, dirs, files in os.walk("scripts"):
					for name in files:
						print os.path.join(root, name)
			except Exception as error:
				print error

	def do_record_macro(self, args):
		"""record macro for later use

		record_macro <name>
		Keyword arguments:
		name     -- macro name

		"""
		return

	def do_play_macro(self, args):
		"""run macro on all active hosts

		play_macro <name>
		Keyword arguments:
		name     -- macro name

		"""
		return

	def do_run(self, args):
		"""run command on all active hosts

		run <command>
		run <hostname,command>
		Keyword arguments:
		hostname -- hostname from the list, needs to be active
		command  -- command to run on host

		"""
		if args:
			argc = len(args.split(','))

			target = None
			if argc == 2:
				target, command = args.split(',')
			elif argc == 1:
				command = args

			if target:
				RunCommand(self.targets[target], command).run()
			else:
				RunCommand(self.targets, command).run()

				for target in self.targets:
					if self.targets[target].enabled:
						print "%s :" % target
						print self.targets[target].log[-1]
		else:
			parse_error(self.do_run, args)

	def do_enable_host(self, args):
		"""activates host for processing

		enable_host <hostname>,hostname,...
		Keyword arguments:
		hostname -- hostname from the list

		"""
		if args:
			for target in args.split(','):
				try:
					self.targets[target].enabled = True
				except KeyError:
					print "host %s not in database" % target
		else:
			parse_error(self.do_enable_host, args)

	def do_disable_host(self, args):
		"""deactivates host for processing

		disable_host <hostname>,hostname,...
		Keyword arguments:
		hostname -- hostname from the list

		"""
		if args:
			for target in args.split(','):
				try:
					self.targets[target].enabled = False
				except KeyError:
					print "host %s not in database" % target
		else:
			parse_error(self.do_disable_host, args)

	def do_disable_all(self, args):
		"""deactivates all hosts for processing

		disable_all
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_disable_all, args)
		else:
			for target in self.targets:
				self.targets[target].enabled = False

	def do_update(self, args):
		"""update all active hosts

		update
		Keyword arguments:
		None

		"""
		for target in self.targets:
			not_installed = []
			packages = self.targets[target].packages
			for package in packages:
				required = self.metadata.packages[package]
				before = self.targets[target].query_version(package)

				packages.set_version(package, 'required', required)
				packages.set_version(package, 'before', before)
				if before == "0":
					not_installed.append(package)
				else:
					if vercmp(before, required) > -1:
						print "warning: %s: package is already updated: %s (%s, required %s)" % (target, package, before, required)

			if len(not_installed):
				print "warning: %s: these packages are not installed: %s" % (target, not_installed)

		if raw_input("start update process? (y/N) ").lower() in ["y", "yes" ]:
			print "updating"
			u = ZypperUpdate(self.targets, self.metadata.patches["sat"])
			u.run()

			for target in self.targets:
				packages = self.targets[target].packages
				for package in packages:
					before = packages.get_version(package, 'before')
					required = packages.get_version(package, 'required')
					after = self.targets[target].query_version(package)
					packages.set_version(package, 'after', after)
					if after != "0":
						if vercmp(before, after) == 0:
							print "warning: %s: package was not updated: %s (%s)" % (target, package, after) 

						if vercmp(after, required) < 0:
							print "warning: %s: package does not match required version: %s (%s, required %s)" % (target, package, after, required) 

	def do_put(self, args):
		"""upload file to all active hosts

		put <local filename>
		Keyword arguments:
		filename -- file to upload to target hosts

		"""
		if args:
			remote = "/tmp/" + os.path.basename(args)

			for target in self.targets:
				if self.targets[target].enabled:
					self.targets[target].connection.put(args, remote)

					print "uploaded %s to %s on %s" % (args, remote, target)
		else:
			parse_error(self.do_put, args)

	def do_get(self, args):
		"""download file from all active hosts

		get <remote filename>
		Keyword arguments:
		filename -- file to download from target hosts

		"""
		if args:
			destination = "downloads/" + self.metadata.md5
			try:
				os.mkdir(destination)
			except Exception as error:
				print error
				return

			for target in self.targets:
				if self.targets[target].enabled:
					local = destination + os.path.basename(args) + "." + target
					self.targets[target].connection.get(args, local)

					print "downloaded %s to %s from %s" % (args, local, target)
		else:
			parse_error(self.do_get, args)

	def do_save(self, args):
		"""save testing log to XML file

		save filename
		Keyword arguments:
		filename -- save log as file filename

		"""
		output = XMLOutput()
		output.add_header(self.metadata)
		for target in self.targets:
			output.add_target(self.targets[target])

		if args:
			try:
				outxml = open(args.split(',')[0], "w")
			except Exception as error:
				print "unable to save file:", error
		else:
			outxml = open(self.metadata.md5 + ".xml", "w")

		outxml.write(output.pretty())
		outxml.close()

	def do_quit(self, args):
		"""disconnects hosts and quits programm

		quit
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_quit, args)
		else:
			for target in self.targets:
				self.targets[target].connection.close()

			sys.exit(0)

def parse_error(method, args):
	print 
	print red("Error parsing command: %s %s" % (method.__name__.replace('do_',''), args))
	print "%s: %s" % (method.__name__.replace('do_',''), method.__doc__)

def green(text):
	return "\033[1;32m%s\033[1;m" % text

def red(text):
	return "\033[1;31m%s\033[1;m" % text

def yellow(text):
	return "\033[1;33m%s\033[1;m" % text

