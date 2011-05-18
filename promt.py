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
import glob

from rpmcmp import *
from target import *
from updater import *
from export import *

out = logging.getLogger('mtui')

class CommandPromt(cmd.Cmd):
 
	prompt = 'QA > '
 
	def __init__(self, targets, metadata):
		cmd.Cmd.__init__(self)
		self.targets = targets
		self.metadata = metadata
		self.systems = []
 
		readline.set_completer_delims('`!@#$%^&*()=+[{]}\|;:",<>? ')
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
		"""
		Adds another machine to the target host list. The system type needs
		to be specified as well.

		add_host <hostname,system>
		Keyword arguments:
		hostname -- address of the target host (should be the FQDN)
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
		"""
		Disconnects from host and remove host from list. Warning: The host
		log is purged as well. If the tester wants to preserve the log, it's
		better to use the '''''set_host_state''''' command instead and set
		the host to "disabled". Multible hosts can be specified.

		remove_host <hostname>[,hostname,...]
		Keyword arguments:
		hostname -- hostname from the target list
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
		"""
		Lists all connected hosts including the system types and their
		current state. State could be "Enabled", "Disabled" or "Dryrun".

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

				system = "(%s)" % self.targets[target].system
				print '{0:20} {1:20}: {2}'.format(target, system, state)

 	def do_list_packages(self, args):
		"""
		Lists current installed package versions from the targets if a
		target is specified. If none is specified, all required package
		versions which should be installed after the update are listed.
		If version 0 is shown for a package, the package is not installed.

		list_packages [hostname]
		Keyword arguments:
		hostname -- hostname or address of the host
		"""

		if args:
			if args.split(',')[0] == 'all':
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
		"""
		List available scripts from the scripts subdirectory. This scripts
		are run in a pre updated state and in the post updated state.

		list_scripts
		Keyword arguments:
		None
		"""

		if args:
			parse_error(self.do_list_scripts, args)

		else:
			try:
				for root, dirs, files in os.walk("%s/scripts" % os.path.dirname(self.metadata.path)):
					for name in files:
						if not ".svn" in root:
							print os.path.join(root, name)
			except Exception as error:
				out.error(str(error))

 	def do_list_update_commands(self, args):
		"""
		List all commands which are invoked when applying updates on the
		target hosts.

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
		"""
		Lists related bugs and corresponding Bugzilla URLs.

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
		"""
		Lists patchinfo metadata like patch number, SWAMP ID or packager.

		list_metadata
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
		"""
		Prints the command protocol from the specified hosts. This might be
		handy for the tester, as one can simply dump the command history to
		the reproducer section of the template.

		show_log [hostname]
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		if args:
			if args.split(',')[0] == 'all':
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
		"""
		record macro for later use

		record_macro <name>
		Keyword arguments:
		name     -- macro name

		"""
		return

	def do_play_macro(self, args):
		"""
		run macro on all active hosts

		play_macro <name>
		Keyword arguments:
		name     -- macro name

		"""
		return

	def do_run(self, args):
		"""
		Runs a command on a specified host or on all enabled targets if
		'all' is given as hostname. The command timeout is set to 10 minutes
		which means, if there's no output on stdout or stderr for 10 minutes,
		a timeout exception is thrown. The commands are run in parallel on
		every target. After the call returned, the output (including the
		return code) of each host is shown on the console.
		Please be aware that no interactive commands can be run with this
		procedure.

		run <hostname,command>
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		if args:
			command = ",".join(args.split(',')[1:])

			if args.split(',')[0] == 'all':
				targets = self.targets
			else:
				targets = selected_targets(self.targets, [args.split(',')[0]])

			if targets:
				try:
					RunCommand(targets, command).run()
				except:
					return

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
		"""
		Sets the host state to "Enabled", "Disabled" or "Dryrun". A host
		set to "Enabled" runs all issued commands while a "Disabled" host
		or a host set to "Dryrun" doesn't run any command on the host.
		The difference between "Disabled" and "Dryrun" is that on "Dryrun"
		hosts the issued commands are printed to the console while "Disabled"
		doesn't print anything. The commands accepts multiple hostnames
		followed by the wanted state.

		set_host_state <hostname>[,hostname,...],<state>
		Keyword arguments:
		hostname -- hostname from the target list
		state    -- enabled, disabled, dryrun
		"""

		if args:
			if args.split(',')[0] == 'all':
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
		"""
		Prints the command protocol from the specified hosts. This might
		be handy for the tester, as one can simply dump the command history
		to the reproducer section of the template.

		show_log [hostname]
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		levels = {"warning":logging.WARNING, "info":logging.INFO, "debug":logging.DEBUG}

		if args in levels.keys():
			out.setLevel(level=levels[args])
		else:
			parse_error(self.do_set_log_level, args)

	def complete_set_log_level(self, text, line, begidx, endidx):
		return [i for i in ['warning', 'info', 'debug'] if i.startswith(text) and i not in line]
			
	def do_set_repo(self, args):
		"""
		Sets the software repositories to UPDATE or TESTING. Multiple
		hostnames can be given. On the target hosts, the rep-clean.sh script
		is spawned to set the repositories accordingly.

		set_repo <hostname>[,hostname,...],<repository>
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		repository -- repository, TESTING or UPDATE
		"""

		if args:
			if args.split(',')[0] == 'all':
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

	def do_downgrade(self, args):
		"""
		Downgrades all related packages to the last released version	(using
		the UPDATE channel). This does not work for SLES 9 hosts, though.

		downgrade <hostname>
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		if args:
			if args.split(',')[0] == 'all':
				targets = self.targets
			else:
				targets = selected_targets(self.targets, args.split(','))

			if targets:
				for release in self.metadata.get_releases():
					try:
						downgrader = Downgrader[release]
					except KeyError:
						out.error("no downgrader available for %s" % release)
						return

					out.info("downgrading")
					try:
						downgrader(targets, self.metadata.get_package_list(), self.metadata.patches).run()
					except:
						out.critical("could not downgrade target systems %s", targets.keys())
						#pass
						raise
					else:
						out.info("done")

		else:
			parse_error(self.do_downgrade, args)

	def complete_downgrade(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

	def do_prepare(self, args):
		"""
		Installs missing packages from the UPDATE repositories. This is
		also run by the update procedure before applying the updates.

		prepare <hostname>
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		if args:
			if args.split(',')[0] == 'all':
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
			parse_error(self.do_prepare, args)

	def complete_prepare(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

	def do_update(self, args):
		"""
		Applies the testing update to the target hosts. While updating the
		machines, the pre-, post- and compare scripts are run before and
		after the update process.

		update <hostname>
		Keyword arguments:
		hostname   -- hostname from the target list or "all"
		"""

		if args:
			if args.split(',')[0] == 'all':
				targets = self.targets
			else:
				targets = selected_targets(self.targets, args.split(','))

			self.do_prepare(args)

			for target in targets:
				not_installed = []
				packages = targets[target].packages

				targets[target].query_versions()

				for package in packages:
					required = self.metadata.packages[package]

					if vercmp(targets[target].packages[package].before, targets[target].packages[package].current) == 1:
						packages[package].set_versions(required=required)
					else:
						packages[package].set_versions(before=targets[target].packages[package].current, required=required)

					before = targets[target].packages[package].before

					if before == "0":
						not_installed.append(package)
					else:
						if vercmp(before, required) > -1:
							out.warning("%s: package is already updated: %s (%s, required %s)" % (target, package, before, required))

				if len(not_installed):
					out.warning("%s: these packages are not installed: %s" % (target, not_installed))

			if input("start pre update scripts? (y/N) ", ["y", "yes" ]):
				script_hook(targets, "pre", os.path.dirname(self.metadata.path), self.metadata.md5)

			if input("start update process? (y/N) ", ["y", "yes" ]):
				out.info("updating")

				release = self.metadata.get_releases()[0]
				try:
					updater = Updater[release]
				except KeyError:
					out.error("no updater available for %s" % release)
					return

				try:
					updater(targets, self.metadata.patches).run()
				except UpdateError as error:
					out.warning("there were errors while updating: %s" % error)
					if input("cancel update process? (y/N) ", ["y", "yes" ]):
						return

				for target in targets:
					packages = targets[target].packages

					targets[target].query_versions()

					for package in packages:
						before = packages[package].before
						required = packages[package].required
						after = targets[target].packages[package].current

						packages[package].set_versions(after=after)

						if after != "0":
							if vercmp(before, after) == 0:
								out.warning("%s: package was not updated: %s (%s)" % (target, package, after))

							if vercmp(after, required) < 0:
								out.warning("%s: package does not match required version: %s (%s, required %s)" % (target, package, after, required))

				if input("start post update scripts? (y/N) ", ["y", "yes" ]):
					script_hook(targets, "post", os.path.dirname(self.metadata.path), self.metadata.md5)

					if input("start compare scripts? (y/N) ", ["y", "yes" ]):
						script_hook(targets, "compare", os.path.dirname(self.metadata.path), self.metadata.md5)

			out.info("done")
		else:
			parse_error(self.do_update, args)

	def complete_update(self, text, line, begidx, endidx):
		return self.complete_hostlist_with_all(text, line, begidx, endidx)

	def do_checkout(self, args):
		"""
		Update template files from the SVN.

		checkout
		Keyword arguments:
		none
		"""

		exitcode = os.system("cd %s; svn up" % os.path.dirname(self.metadata.path))

		if exitcode != 0:
			out.error("updating template failed, returncode: %s" % exitcode)

	def do_commit(self, args):
		"""
		Commits the testing template to the SVN. This can be run after the
		testing has finished an the template is in the final state.

		commit
		Keyword arguments:
		none
		"""

		exitcode = os.system("cd %s; svn up; svn ci" % os.path.dirname(self.metadata.path))

		if exitcode != 0:
			out.error("committing template failed, returncode: %s" % exitcode)

	def do_put(self, args):
		"""
		Uploads files to all enabled hosts. Multiple files can be selected
		with special patterns according to the rules used by the Unix shell
		(i.e. *, ?, []). The complete filepath on the remote hosts is shown
		after the upload. put has also directory completion.

		put <local filename>
		Keyword arguments:
		filename -- file to upload to the target hosts
		"""

		if args:
			for filename in glob.glob(args):
				if os.path.isfile(filename):
					remote = "/tmp/%s/%s" % (self.metadata.md5, os.path.basename(filename))

					try:
						FileUpload(self.targets, filename, remote).run()
					except:
						out.error("uploading %s to %s failed" % (filename, remote))
					else:
						out.info("uploaded %s to %s" % (filename, remote))

		else:
			parse_error(self.do_put, args)

	def complete_put(self, text, line, begidx, endidx):
		return self.complete_filelist(text, line, begidx, endidx)

	def do_get(self, args):
		"""
		Downloads a file from all enabled hosts. Multiple files can not be
		selected. Files are saved in the downloads/$md5/ subdirectory with the
		hostname as file extension.

		get <remote filename>
		Keyword arguments:
		filename -- file to download from the target hosts
		"""

		if args:
			destination = os.path.dirname(self.metadata.path) + "/downloads/"
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

	def do_edit(self, args):
		"""
		Edit a local file or the testing template. The evironment variable
		EDITOR is processed to find the prefered editor. If EDITOR is empty,
		"vi" is set as default.

		edit file,<filename>
		edit template
		Keyword arguments:
		filename -- edit filename
		template -- edit template
		"""

		command = args.split(",")[0]

		editor = os.environ.get("EDITOR", "vi")

		if command == "file":
			os.system("%s %s" % (editor, args.split(",")[1]))
		elif command == "template":
			os.system("%s %s" % (editor, self.metadata.path))
		else:
			parse_error(self.do_edit, args)

	def complete_edit(self, text, line, begidx, endidx):
		if "file," in line:
			return self.complete_filelist(text.replace("file,", "", 1), line, begidx, endidx)
		else:
			return [i for i in ["file", "template"] if i.startswith(text)]

	def do_export(self, args):
		"""
		Exports the gathered update data to template file. This includes
		the pre/post package versions and the update log. An output file could
		be specified, if none is specified, the output is written to the
		current testing template.

		export [filename]
		Keyword arguments:
		filename -- output template file name
		"""

		if args:
			filename = args.split(',')[0]
		else:
			filename = self.metadata.path

		output = XMLOutput()
		output.add_header(self.metadata)

		for target in self.targets:
			output.add_target(self.targets[target])

		try:
			template = xml_to_template(self.metadata.path, output.pretty())
		except Exception:
			out.error("could not export XML")
			return

		if os.path.exists(filename):
			out.warning("file %s exists." % filename)
			if not input("should i overwrite %s? (y/N) " % filename, ["y", "yes" ]):
				filename = add_time(filename)

		out.info("exporting XML to %s" % filename)
		try:
			with open(filename, 'w') as f:
				f.write("".join(l.encode("utf-8") for l in template))
		except Exception as error:
			print "failed to write %s: %s" % (filename, str(error))
		else:
			print "wrote template to %s" % filename

	def do_save(self, args):
		"""
		Save the testing log to a XML file. All commands and package
		versions are saved there. When no parameter is given, the XML is saved
		to output/$md5/log.xml. If that file already exists and the tester
		doesn't want to overwrite it, a postfix (current timestamp) is added
		to the filename. The log can be used to fill the required sections
		of the testing template after the testing has finished.
		This could be done with the convert.py script.

		save [filename]
		Keyword arguments:
		filename -- save log as file filename
		"""

		if args:
			filename = args.split(',')[0]
		else:
			filename = "log.xml"

		output_dir = os.path.dirname(self.metadata.path) + "/output/"

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
			if not input("should i overwrite %s? (y/N) " % filename, ["y", "yes" ]):
				filename = add_time(filename)

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
		"""
		Disconnects from all hosts and exits the programm.
		The tester is asked to save the XML log when exiting MTUI.

		quit
		Keyword arguments:
		None
		"""

		if args:
			parse_error(self.do_quit, args)
		else:
			if input("save log? (y/N) ", ["y", "yes" ]):
				self.do_save(None)

			for target in self.targets:
				self.targets[target].close()

			readline.write_history_file(".mtui_history")
			sys.exit(0)

	def complete_filelist(self, text, line, begidx, endidx):
		dirname = ""
		filename = ""

		if text.startswith("~"):
			text = text.replace("~", os.path.expanduser('~'), 1)
			text += "/"

		if "/" in text:
			dirname = "/".join(text.split("/")[:-1])
			dirname += "/"

		if not dirname:
			dirname = "./"

		filename = text.split("/")[-1]

		return [dirname + i for i in os.listdir(dirname) if i.startswith(filename)]

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

def script_hook(targets, which, scriptdir, md5):
	if which not in ["post", "pre", "compare"]:
		return

	output_dir = "%s/output/scripts" % scriptdir
	remote_dir = "/tmp/%s" % md5
	
	for script in os.listdir("%s/scripts/%s" % (scriptdir, which)):
		local_file = "%s/scripts/%s/%s" % (scriptdir, which, script)
		remote_file = "%s.%s" % (which, script)

		if not os.path.isfile(local_file):
			continue

		out.info("preparing script %s" % script)

		try:
			if which == "compare":
				for target in targets:
					prename = "%s/pre.%s.%s" % (output_dir, script.replace("compare_", "check_"), target)
					postname = "%s/post.%s.%s" % (output_dir, script.replace("compare_", "check_"), target)
					command = ["%s/scripts/compare/%s" % (scriptdir, script), prename, postname]
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
		except KeyboardInterrupt:
			out.warning("skipping script %s" % script)
			continue

def add_time(value):
	import math
	import time

	value += "." + str(math.trunc(time.time()))

	return value

def input(text, options):
	try:
		if raw_input(text).lower() in options:
			return True
		else:
			return False
	except KeyboardInterrupt:
		return False

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


