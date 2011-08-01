#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import errno
import getopt
import logging
import shutil

from log import *
from promt import *
from template import *

out = logging.getLogger('mtui')

def main():
	"""parsing parameter list and initializing metadata"""

	md5 = None
	team = None
	directory = os.getenv("TEMPLATEDIR", ".")
	interactive = False
	dryrun = False
	timeout = None

	targets = {}

	try:
		opts, args = getopt.getopt(sys.argv[1:], "ihaedt:m:vw:", ["interactive", "help", "asia", "emea", "dryrun", "templates=", "md5=", "verbose", "timeout"])
	except getopt.GetoptError as error:
		out.error(str(error))
		usage()

	for parameter, argument in opts:
		if parameter in ("-h", "--help"):
			usage()
		elif parameter in ("-a", "--asia"):
			team = "asia"
		elif parameter in ("-e", "--emea"):
			team = "emea"
		elif parameter in ("-m", "--md5"):
			md5 = argument
		elif parameter in ("-d", "--dryrun"):
			dryrun = True
		elif parameter in ("-t", "--templates"):
			directory = argument
		elif parameter in ("-i", "--interactive"):
			interactive = True
		elif parameter in ("-v", "--verbose"):
			out.setLevel(level=logging.DEBUG)
		elif parameter in ("-w", "--timeout"):
			try:
				timeout = int(argument)
			except Exception:
				out.error("wrong timeout value")
				sys.exit(0)
		else:
			usage()

	if md5 is None:
		out.error("please specify an update identifier")
		usage()

	try:
		update = Template(md5, team, directory)
	except IOError:
		if raw_input("Template does not yet exist. Try to check it out? (y/N) ").lower() in ["y", "yes"]:
			os.system("cd %s; svn co svn+ssh://svn@qam.suse.de/testreports/%s" % (directory, md5))
			try:
				update = Template(md5, team, directory)
			except Exception:
				sys.exit(0)
		else:
			sys.exit(0)
	except Exception:
		usage()

	metadata = update.metadata

	for host, system in metadata.systems.items():
		try:
			targets[host] = Target(host, system, metadata.get_package_list(), dryrun=dryrun, timeout=timeout)
		except Exception:
			out.warning("could not add host %s to target list" % host)
		except KeyboardInterrupt:
			out.warning("skipping host %s" % host)

	ignored = shutil.ignore_patterns("*.svn")

	try:
		shutil.copytree("%s/scripts" % os.path.dirname(__file__), "%s/scripts" % os.path.dirname(metadata.path), ignore=ignored)
	except OSError as error:
		if error.errno == errno.ENOENT:
			out.warning("scripts/ dir not found, please copy manually")
		else:
			pass

	promt = CommandPromt(targets, metadata)

	try:
		if interactive:
			promt.cmdloop()
		else:
			promt.do_update("all")
			promt.do_export(None)
			promt.do_quit(None)
	except KeyboardInterrupt:
		promt.do_quit(None)

def usage():
	print
	print "Maintenance Test Update Installer"
	print "=" * 35
	print
	print sys.argv[0], "[parameter] {-m|--md5 update}"
	print
	print "parameters:"
	print "\t-{short},--{long:20}{description}".format(short="a", long="asia", description="use asia template")
	print "\t-{short},--{long:20}{description}".format(short="e", long="emea", description="use emea template")
	print "\t-{short},--{long:20}{description}".format(short="t", long="template=", description="template directory")
	print "\t-{short},--{long:20}{description}".format(short="m", long="md5=", description="md5 update identifier")
	print "\t-{short},--{long:20}{description}".format(short="i", long="interactive", description="interactive update shell")
	print "\t-{short},--{long:20}{description}".format(short="d", long="dryrun", description="start in dryrun mode")
	print "\t-{short},--{long:20}{description}".format(short="v", long="verbose", description="enable debugging output")
	print "\t-{short},--{long:20}{description}".format(short="w", long="timeout", description="execution timeout in seconds")
	print

	sys.exit(0)
