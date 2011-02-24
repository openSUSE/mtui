#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import sys
import getopt

from template import *

def main():
	"""main() looks like a mess. main() is a mess. migrating to interactive mode"""
	md5 = None
	team = None
	directory = None
	interactive = False

	targets = {}

	try:
		opts, args = getopt.getopt(sys.argv[1:], "ihaed:m:", ["interactive", "help", "asia", "emea", "dir=", "md5="])
	except getopt.GetoptError as error:
		print str(error)
		usage()
		sys.exit(-1)

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
		elif parameter in ("-d", "--dir"):
			directory = argument
		elif parameter in ("-i", "--interactive"):
			interactive = True
		else:
			assert False, "unhandled parameter"

	if md5 == None:
		print "please specify an update identifier"
		usage()

	update = Template(md5, team, directory)
	metadata = update.metadata

	for host, system in metadata.systems.items():
		try:
			targets[host] = Target(host, system, metadata.get_package_list())
		except Exception as error:
			print "could not add host %s to target list:" % host, error

	if raw_input("should i first try to install missing packages? ").lower() in [ "y", "yes" ]:
		print "installing"
		ZypperPrepare(targets, metadata.get_package_list()).run()

	if interactive:
		promt = CommandPromt(targets, metadata)
		promt.cmdloop()
	else:
		for host in targets:
			not_installed = []
			packages = targets[host].packages
			for package in packages:
				required = metadata.packages[package]
				before = targets[host].query_version(package)

				packages.set_version(package, 'required', required)
				packages.set_version(package, 'before', before)
				if before == "0":
					not_installed.append(package)
				else:
					if vercmp(before, required) > -1:
						print "warning: %s: package is already updated: %s (%s, required %s)" % (host, package, before, required)

			if len(not_installed):
				print "warning: %s: these packages are not installed: %s" % (host, not_installed)

		if raw_input("start pre update scripts? (y/N) ").lower() in ["y", "yes" ]:
			for script in os.listdir("scripts/pre"):
				print "preparing script", script
				remote = "pre.%s" % script

				FileUpload(targets, "scripts/pre/%s" % script, "/tmp/%s" % remote).run()
				RunCommand(targets, "/tmp/%s" % remote).run()

				for target in targets:
					f = open("output/%s.%s" % (remote, target), "w")
					f.write(targets[target].log[-1][2])
					f.close()

		if raw_input("start update process? (y/N) ").lower() in ["y", "yes" ]:
			print "updating"
			ZypperUpdate(targets, metadata.patches["sat"]).run()

			for host in targets:
				packages = targets[host].packages
				for package in packages:
					before = packages.get_version(package, 'before')
					required = packages.get_version(package, 'required')
					after = targets[host].query_version(package)
					packages.set_version(package, 'after', after)
					if after != "0":
						if vercmp(before, after) == 0:
							print "warning: %s: package was not updated: %s (%s)" % (host, package, after) 

						if vercmp(after, required) < 0:
							print "warning: %s: package does not match required version: %s (%s, required %s)" % (host, package, after, required) 

			if raw_input("start post update scripts? (y/N) ").lower() in ["y", "yes" ]:
				for script in os.listdir("scripts/post"):
					print "preparing script", script
					remote = "post.%s" % script

					FileUpload(targets, "scripts/post/%s" % script, "/tmp/%s" % remote).run()
					RunCommand(targets, "/tmp/%s" % remote).run()

					for target in targets:
						f = open("output/%s.%s" % (remote, target), "w")
						f.write(targets[target].log[-1][2])
						f.close()

			if raw_input("compare script output? (y/N) ").lower() in ["y", "yes" ]:
				print

			output = XMLOutput()
			output.add_header(metadata)
			for host in targets:
				output.add_target(targets[host])

			outxml = open(metadata.md5 + ".xml", "w")
			outxml.write(output.pretty())
			outxml.close()

		else:
			print "not updating"

	for target in targets:
		targets[target].connection.close()

def usage():
	print
	print sys.argv[0], "-a,--asia asia hosts"
	print sys.argv[0], "-e,--emea emea hosts (default)"
	print sys.argv[0], "-d,--dir template directory"
	print sys.argv[0], "-m,--md5 update md5 identifier"
	print sys.argv[0], "-i,--interactive update shell"
	print

