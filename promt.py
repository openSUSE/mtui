#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import stat
import errno
import cmd
import logging
import readline
import subprocess

from rpmcmp import *
from target import *
from updater import *

out = logging.getLogger('mtui')

class CommandPromt(cmd.Cmd):
 
	prompt = 'QA > '
 
	def __init__(self, targets, metadata):
		cmd.Cmd.__init__(self)
		self.targets = targets
		self.metadata = metadata
		self.systems = []
 
		readline.set_completer_delims('`~!@#$%^&*()=+[{]}\|;:",<>/? ')
		try:
			readline.read_history_file(".mtui_history")
		except IOError:
			pass

		try:
			with open('system.list', 'r') as f:
				self.systems = f.readlines()
		except:
			pass

	def do_add_host(self, args):
		"""connect to another host and add it to the list

		add_host <hostname>,<system>
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

			try:
				self.targets[hostname] = Target(hostname, system, self.metadata.get_package_list())
			except:
				out.error("unable to add host %s to list" % hostname)

		else:
			parse_error(self.do_add_host, args)
 
	def complete_add_host(self, text, line, begidx, endidx):
		return self.complete_systemlist(text, line, begidx, endidx)
		
	def do_remove_host(self, args):
		"""disconnect from host and remove host from list

		remove_host <hostname>,hostname,...
		Keyword arguments:
		hostname -- hostname or address of the host

		"""
		if args:
			for target in args.split(','):
				try:
					self.targets[target].close()
					del self.targets[target]
				except KeyError:
					out.warning("host %s not in database" % target)

		else:
			parse_error(self.do_remove_host, args)

	def complete_remove_host(self, text, line, begidx, endidx):
		return self.complete_hostlist(text, line, begidx, endidx)
 
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
				if self.targets[target].state == "enabled":
					state = green('Enabled')
				elif self.targets[target].state == "dryrun":
					state = yellow('Dryrun')
				else:
					state = red('Disabled')

				print '{0:30}: {1}'.format(target, state)

 	def do_list_packages(self, args):
		"""list packages from template or from target if specified

		list_packages hostname
		Keyword arguments:
		hostname -- hostname or address of the host

		"""
		if args:
			if 'all' in args:
				targets = list(self.targets)
			else:
				targets = args.split(',')

			for target in targets:
				try:
					self.targets[target].query_versions()
				except KeyError:
					out.warning("host %s not in database" % target)
					targets.remove(target)
				else:
					print "packages on %s:" % target
					for package in self.targets[target].packages:
						print '{0:30}: {1}'.format(package, self.targets[target].packages[package].current)

				print

		else:
			for package, version in self.metadata.packages.items():
				print '{0:30}: {1}'.format(package, version)

	def complete_list_packages(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

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
				out.error(str(error))

 	def do_list_update_commands(self, args):
		"""list commands which are invoked to apply update

		list_update_commands
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_list_update_commands, args)

		else:
			for release in self.metadata.get_releases():
				try:
					updater = Updater[release]
				except KeyError:
					out.error("no updater available for %s" % release)
					return

				print "\n".join(updater(self.targets, self.metadata.patches).commands)
				del updater

 	def do_list_bugs(self, args):
		"""list bugs and bugzilla URLs

		list_bugs
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_list_bugs, args)

		else:
			for bug, description in self.metadata.bugs.items():
				print 'Bug #{0:5}: {1}'.format(bug, description)
				print 'https://bugzilla.novell.com/show_bug.cgi?id=%s' % bug
				print

 	def do_list_metadata(self, args):
		"""list update metadata

		list_bugs
		Keyword arguments:
		None

		"""
		if args:
			parse_error(self.do_list_metadata, args)

		else:
			print '{0:15}: {1}'.format("MD5SUM", self.metadata.md5)
			print '{0:15}: {1}'.format("SWAMP ID", self.metadata.swampid)
			print '{0:15}: {1}'.format("Category", self.metadata.category)
			print '{0:15}: {1}'.format("Packager", self.metadata.packager)
			for type, id in self.metadata.patches.items():
				print '{0:15}: {1}'.format(type.upper(), id)

	def do_show_log(self, args):
		"""show command protocol

		show_log
		Keyword arguments:
		hostname -- hostname of the target host

		"""
		if args:
			if 'all' in args:
				targets = list(self.targets)
			else:
				targets = args.split(',')

			for target in targets:
				print "log from %s:" % target
				try:
					for line in self.targets[target].log:
						print "%s:~> %s [%s]" % (target, line[0], line[3])
						print "stdout:"
						print "%s" % (line[1])
						print "stderr:"
						print "%s" % (line[2])
						print
				except KeyError:
					out.warning("host %s not in database" % target)

		else:
			parse_error(self.do_show_log, args)

	def complete_show_log(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

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
		"""run command on active hosts

		run <hostname,command>
		Keyword arguments:
		hostname -- hostname from the list
		command  -- command to run on host

		"""
		if args:
			command = ",".join(args.split(',')[1:])

			if 'all' in args:
				targets = list(self.targets)
			else:
				targets = selected_targets(self.targets, [args.split(',')[0]])

			RunCommand(self.targets, command).run()

			for target in targets:
				if target in self.targets and self.targets[target].state == "enabled":
					print "%s:~> %s [%s]" % (target, self.targets[target].lastin(), self.targets[target].lastexit())
					print self.targets[target].lastout()
					if self.targets[target].lasterr():
						print "stderr:", self.targets[target].lasterr()

			out.info("done")
		else:
			parse_error(self.do_run, args)

	def complete_run(self, text, line, begidx, endidx):
		return [i for i in list(self.targets) + ['all'] if i.startswith(text) and not line.count(',')]

	def do_set_host_state(self, args):
		"""sets host state to enabled, disabled or dryrun

		set_host_state <hostname>,hostname,...,<state>
		Keyword arguments:
		hostname -- hostname from the list
		state    -- enabled, disabled, dryrun

		"""
		if args:
			if 'all' in args:
				targets = list(self.targets)
			else:
				targets = args.split(',')[:-1]

			state = args.split(',')[-1]

			if state not in ['enabled', 'disabled', 'dryrun']:
				parse_error(self.do_set_host_state, args)
				return

			for target in targets:
				try:
					self.targets[target].state = state

				except KeyError:
					out.info("host %s not in database" % target)
					targets.remove(target)

		else:
			parse_error(self.do_set_host_state, args)

	def complete_set_host_state(self, text, line, begidx, endidx):
		if line.count(','):
			return [i for i in list(self.targets) + ['enabled', 'disabled', 'dryrun'] if i.startswith(text) and i not in line]
		else:		
			return self.complete_hostlist_with_all(text, line, begidx, endidx)

	def do_set_log_level(self, args):
		"""sets mtui log level (default: info)

		set_log_level <loglevel>
		Keyword arguments:
		loglevel -- warning, info or debug

		"""
		levels = {"warning":logging.WARNING, "info":logging.INFO, "debug":logging.DEBUG}

		if args in levels.keys():
			out.setLevel(level=levels[args])
		else:
			parse_error(self.do_set_log_level, args)

	def complete_set_log_level(self, text, line, begidx, endidx):
		return [i for i in ['warning', 'info', 'debug'] if i.startswith(text) and i not in line]
			
	def do_set_repo(self, args):
		"""sets software repository to UPDATE or TESTING

		set_repo <hostname>,hostname,...,<repository>
		Keyword arguments:
		hostname   -- hostname from the list or "all"
		repository -- repository, TESTING or UPDATE

		"""
		if args:
			if 'all' in args:
				targets = list(self.targets)
			else:
				targets = args.split(',')[:-1]

			name = args.split(',')[-1]

			if name not in ['testing', 'update']:
				parse_error(self.do_set_repo, args)
				return

			for target in targets:
				try:
					self.targets[target].set_repo(name.upper())

				except KeyError:
					out.info("host %s not in database" % target)
					targets.remove(target)

		else:
			parse_error(self.do_set_repo, args)

	def complete_set_repo(self, text, line, begidx, endidx):
		if line.count(','):
			return [i for i in list(self.targets) + ['testing', 'update'] if i.startswith(text) and i not in line]
		else:		
			return [i for i in list(self.targets) + ['all'] if i.startswith(text) and i not in line]

	def do_prepare_hosts(self, args):
		"""install missing packages on hosts

		prepare_hosts
		Keyword arguments:
		None

		"""
		if args:
			if 'all' in args:
				targets = self.targets
			else:
				targets = selected_targets(self.targets, args.split(','))

			if targets:
				for release in self.metadata.get_releases():
					try:
						preparer = Preparer[release]
					except KeyError:
						out.error("no preparer available for %s" % release)
						return

					out.info("preparing")
					try:
						preparer(targets, self.metadata.get_package_list()).run()
					except:
						out.critical("could not prepare target systems %s", targets.keys())
						pass
					else:
						out.info("done")

		else:
			parse_error(self.do_prepare_hosts, args)

	def complete_prepare_hosts(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

	def do_update(self, args):
		"""update all active hosts

		update
		Keyword arguments:
		None

		"""
		self.do_prepare_hosts("all")

		for target in self.targets:
			not_installed = []
			packages = self.targets[target].packages

			self.targets[target].query_versions()

			for package in packages:
				before = self.targets[target].packages[package].current
				required = self.metadata.packages[package]

				packages[package].set_versions(before=before, required=required)

				if before == "0":
					not_installed.append(package)
				else:
					if vercmp(before, required) > -1:
						out.warning("%s: package is already updated: %s (%s, required %s)" % (target, package, before, required))

			if len(not_installed):
				out.warning("%s: these packages are not installed: %s" % (target, not_installed))

		if raw_input("start pre update scripts? (y/N) ").lower() in ["y", "yes" ]:
			script_hook(self.targets, "pre", self.metadata.md5)

		if raw_input("start update process? (y/N) ").lower() in ["y", "yes" ]:
			out.info("updating")

			release = self.metadata.get_releases()[0]
			try:
				updater = Updater[release]
			except KeyError:
				out.error("no updater available for %s" % release)
				return

			updater(self.targets, self.metadata.patches).run()

			for target in self.targets:
				packages = self.targets[target].packages

				self.targets[target].query_versions()

				for package in packages:
					before = packages[package].before
					required = packages[package].required
					after = self.targets[target].packages[package].current

					packages[package].set_versions(after=after)

					if after != "0":
						if vercmp(before, after) == 0:
							out.warning("%s: package was not updated: %s (%s)" % (target, package, after))

						if vercmp(after, required) < 0:
							out.warning("%s: package does not match required version: %s (%s, required %s)" % (target, package, after, required))

			if raw_input("start post update scripts? (y/N) ").lower() in ["y", "yes" ]:
				script_hook(self.targets, "post", self.metadata.md5)

				if raw_input("start compare scripts? (y/N) ").lower() in ["y", "yes" ]:
					script_hook(self.targets, "compare", self.metadata.md5)

		out.info("done")

	def do_put(self, args):
		"""upload file to all active hosts

		put <local filename>
		Keyword arguments:
		filename -- file to upload to target hosts

		"""
		if os.path.isfile(args):
			remote = "/tmp/" + os.path.basename(args)

			try:
				FileUpload(self.targets, args, remote).run()
			except:
				out.error("uploading %s to %s failed" % (args, remote))
			else:
				out.info("uploaded %s to %s" % (args, remote))

		else:
			parse_error(self.do_put, args)

	def complete_put(self, text, line, begidx, endidx):
		return [i for i in os.listdir('.') if i.startswith(text)]

	def do_get(self, args):
		"""download file from all active hosts

		get <remote filename>
		Keyword arguments:
		filename -- file to download from target hosts

		"""
		if args:
			destination = "downloads/" + self.metadata.md5 + '/'
			local = destination + os.path.basename(args)

			try:
				os.makedirs(destination)
			except OSError as exc:
				if exc.errno == errno.EEXIST:
					pass
			except Exception as error:
				out.error(str(error))
				return

			try:
				FileDownload(self.targets, args, local, True).run()
			except Exception as error:
				out.error(str(error))
				out.error("downloading %s to %s failed" % (args, local))
			else:
				out.info("downloaded %s to %s" % (args, local))

		else:
			parse_error(self.do_get, args)

	def do_save(self, args):
		"""save testing log to XML file

		save filename
		Keyword arguments:
		filename -- save log as file filename

		"""
		if args:
			filename = args.split(',')[0]
		else:
			filename = "log.xml"

		output_dir = "output/%s/" % self.metadata.md5

		try:
			os.makedirs(output_dir)
		except OSError as exc:
			if exc.errno == errno.EEXIST:
				pass
		except Exception as error:
			out.error(str(error))
			return

		filename = output_dir + filename

		if os.path.exists(filename):
			out.warning("file %s exists." % filename)
			if not raw_input("should i overwrite %s? (y/N) " % filename).lower() in ["y", "yes" ]:
				import math
				import time

				filename += "." + str(math.trunc(time.time()))
				out.info("saving output to %s" % filename)

		try:
			outxml = open(filename, "w")
		except Exception as error:
			out.error("unable to open file for writing: %s" % str(error))

		output = XMLOutput()
		output.add_header(self.metadata)
		for target in self.targets:
			output.add_target(self.targets[target])

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
			if raw_input("save log? (y/N) ").lower() in ["y", "yes" ]:
				self.do_save(None)

			for target in self.targets:
				self.targets[target].close()

			readline.write_history_file(".mtui_history")
			sys.exit(0)

	def complete_systemlist(self, text, line, begidx, endidx):
		return [i.strip('\n') for i in self.systems if i.startswith(text) and i not in line]

	def complete_hostlist(self, text, line, begidx, endidx):
		return [i for i in self.targets if i.startswith(text) and i not in line]

	def complete_hostlist_with_all(self, text, line, begidx, endidx):
		return [i for i in list(self.targets) + ['all'] if i.startswith(text) and i not in line]

def parse_error(method, args):
	print 
	out.error("failed to parse command: %s %s" % (method.__name__.replace('do_',''), args))
	print "%s: %s" % (method.__name__.replace('do_',''), method.__doc__)

def script_hook(targets, which, md5):
	if which not in ["post", "pre", "compare"]:
		return

	output_dir = "output/%s/scripts" % md5
	remote_dir = "/tmp/%s" % md5
	
	for script in os.listdir("scripts/%s" % which):
		out.info("preparing script %s" % script)
		local_file = "scripts/%s/%s" % (which, script)
		remote_file = "%s.%s" % (which, script)

		if not os.path.isfile(local_file):
			continue

		if which == "compare":
			for target in targets:
				prename = "%s/pre.%s.%s" % (output_dir, script.replace("compare_", "check_"), target)
				postname = "%s/post.%s.%s" % (output_dir, script.replace("compare_", "check_"), target)
				command = ["scripts/compare/%s" % script, prename, postname]
				out.debug("running %s" % str(command))
				sub = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
				exitcode = sub.wait()

				if exitcode == 1:
					out.warning("testcase %s failed: %s" % (script, str(command))) 
				if exitcode == 2:
					out.warning("internal error in testcase %s: %s" % (script, str(command))) 

				targets[target].log.append([" ".join(command), sub.stdout.readlines(), sub.stderr.readlines(), exitcode])

		else:
			FileUpload(targets, local_file, "%s/%s" % (remote_dir, remote_file)).run()
			RunCommand(targets, "%s/%s" % (remote_dir, remote_file)).run()

			try:
				os.makedirs(output_dir)
			except OSError as exc:
				if exc.errno == errno.EEXIST:
					pass
			except Exception as error:
				out.error(str(error))
				return

			for target in targets:
				filename = "%s/%s.%s" % (output_dir, remote_file, target)
				try:
					f = open(filename, "w")
					f.write(targets[target].lastout())
					f.write(targets[target].lasterr())
				except Exception as error:
					out.error("unable to write script output to %s: %s" % (filename, error))
				else:
					f.close()

def selected_targets(targets, target_list):
	temporary_targets = {}

	for target in target_list:
		try:
			temporary_targets[target] = targets[target]
		except KeyError:
			out.info("host %s not in database" % target)

	return temporary_targets
	
def green(text):
	return "\033[1;32m%s\033[1;m" % text

def red(text):
	return "\033[1;31m%s\033[1;m" % text

def yellow(text):
	return "\033[1;33m%s\033[1;m" % text


