#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import getopt
import logging

from log import *
from promt import *
from template import *

out = logging.getLogger('mtui')

def main():
	"""parsing parameter list and initializing metadata"""

	md5 = None
	team = None
	directory = "."
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
			sys.exit(0)
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
			assert False, "unhandled parameter"

	if md5 == None:
		out.error("please specify an update identifier")
		usage()

	update = Template(md5, team, directory)
	metadata = update.metadata

	for host, system in metadata.systems.items():
		try:
			targets[host] = Target(host, system, metadata.get_package_list(), dryrun=dryrun)
		except:
			out.warning("could not add host %s to target list" % host)

	promt = CommandPromt(targets, metadata)
	if interactive:
		promt.cmdloop()
	else:
		promt.do_update(None)
		promt.do_save(None)
		promt.do_quit(None)

def usage():
	print
	print "Maintenance Test Update Installer"
	print "=" * 35
	print
	print sys.argv[0], "<parameter>"
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
