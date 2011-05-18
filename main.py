#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
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

	targets = {}

	try:
		opts, args = getopt.getopt(sys.argv[1:], "ihaedt:m:v", ["interactive", "help", "asia", "emea", "dryrun", "templates=", "md5=", "verbose"])
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
		else:
			usage()

	if md5 is None:
		out.error("please specify an update identifier")
		usage()

	try:
		update = Template(md5, team, directory)
	except IOError:
		if raw_input("Template does not yet exist. Try to check it out? ").lower() in ["y", "yes"]:
			os.system("cd %s; svn co svn+ssh://svn@qam.suse.de/testreports/%s" % (directory, md5))
			try:
				update = Template(md5, team, directory)
			except:
				sys.exit(0)
		else:
			sys.exit(0)
	except:
		usage()

	metadata = update.metadata

	for host, system in metadata.systems.items():
		try:
			targets[host] = Target(host, system, metadata.get_package_list(), dryrun=dryrun)
		except:
			out.warning("could not add host %s to target list" % host)

	ignored = shutil.ignore_patterns("*.svn")
	try:
		shutil.copytree("scripts", "%s/scripts" % os.path.dirname(metadata.path), ignore=ignored)
	except OSError:
		pass

	promt = CommandPromt(targets, metadata)

	try:
		if interactive:
			promt.cmdloop()
		else:
			promt.do_update(None)
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
	print

	sys.exit(0)
