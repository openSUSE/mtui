#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import logging
import codecs
import xml.dom.minidom

from rpmver import *

out = logging.getLogger('mtui')

def xml_to_template(template, xmldata):
	"""
	simple method to export package versions and
	update log from the log to the template file
	"""

	with codecs.open(template, 'r', 'utf-8') as f:
		t = f.readlines()

	try:
		if os.path.isfile(xmldata):
			x = xml.dom.minidom.parse(xmldata)
		else:
			x = xml.dom.minidom.parseString(xmldata)

	except Exception as error:
		out.error("failed to parse XML data: %s" % str(error))
		raise AttributeError("XML")

	for host in x.getElementsByTagName("host"):
		hostname = host.getAttribute("hostname")
		systemtype = host.getAttribute("system")

		line = "%s (reference host: %s)\n" % (systemtype, hostname)
		try:
			i = t.index(line)
		except ValueError:
			out.debug("host section %s not found, searching system" % hostname)
			line = "%s (reference host: ?)\n" % systemtype
			try:
				i = t.index(line)
				t[i] = "%s (reference host: %s)\n" % (systemtype, hostname)
			except ValueError:
				out.debug("system section %s not found, creating new one" % systemtype)
				line = "Test results by product-arch:\n"

				try:
					i = t.index(line) + 2
				except ValueError:
					our.error("update results section not found")
					break

				t.insert(i, "\n")
				i += 1
				t.insert(i, "%s (reference host: %s)\n" % (systemtype, hostname))
				i += 1
				t.insert(i, "--------------\n")
				i += 1
				t.insert(i, "before:\n")
				i += 1
				t.insert(i, "after:\n")
				i += 1
				t.insert(i, "scripts:\n")
				i += 1
				t.insert(i, "\n")
				i += 1
				t.insert(i, "=> PASSED/FAILED\n")
				i += 1
				t.insert(i, "\n")
				i += 1
				t.insert(i, "comment: (none)\n")
				i += 1
				t.insert(i, "\n")

	for host in x.getElementsByTagName("host"):
		versions = {}
		hostname = host.getAttribute("hostname")
		systemtype = host.getAttribute("system")

		line = "%s (reference host: %s)\n" % (systemtype, hostname)
		try:
			i = t.index(line)
		except ValueError:
			out.warning("host section %s not found" % hostname)
			continue
		for state in ["before", "after"]:
			versions[state] = {}
			try:
				i = t.index("      %s:\n" % state, i) + 1
			except ValueError:
				try:
					i = t.index("%s:\n" % state, i) + 1
				except ValueError:
					out.error("%s packages section not found" % state)
					continue

			for package in host.getElementsByTagName(state):
				for child in package.childNodes:
					try:
						name = child.getAttribute("name")
						version = child.getAttribute("version")
						versions[state].update({ name: version })

						if "None" in version:
							break

						if name in t[i]:
							if version != "0":
								t[i] = "\t%s-%s\n" % (name, version)
							else:
								t[i] = "\tpackage %s is not installed\n" % name
						else:
							if version != "0":
								t.insert(i, "\t%s-%s\n" % (name, version))
							else:
								t.insert(i, "\tpackage %s is not installed\n" % name)
						i += 1
					except Exception:
						pass
		try:
			i = t.index("scripts:\n", i, i + 3) + 1
		except ValueError:
			out.debug("scripts section not found, adding one")
			t.insert(i, "      scripts:\n")
			i += 1

		log = host.getElementsByTagName("log")[0]

		failed = 0
		for package in versions["before"].keys():
			if RPMVersion(versions["before"][package]) >= RPMVersion(versions["after"][package]):
				failed = 1
		if failed == 1:
			out.warning("installation test result on %s set to FAILED as some packages were not updated. please override manually." % hostname)

		for child in log.childNodes:
			try:
				name = child.getAttribute("name")
				exitcode = child.getAttribute("return")
			except Exception:
				continue

			if "scripts/compare/compare_" in name:
				scriptname = os.path.basename(name.split(" ")[0])
				scriptname = scriptname.replace("compare_", "")
				scriptname = scriptname.replace(".pl", "")
				scriptname = scriptname.replace(".sh", "")

				if exitcode == "0":
					result = "SUCCEEDED"
				elif exitcode == "1":
					failed = 1
					result = "FAILED"
				else:
					failed = 1
					result = "INTERNAL ERROR"

				t.insert(i, "\t{0:25}: {1}\n".format(scriptname, result))
				i += 1

		if "PASSED/FAILED" in t[i+1]:
			if failed == 0:
				t[i+1] = "=> PASSED\n"
			elif failed == 1:
				t[i+1] = "=> FAILED\n"
			

	try:
		i = t.index("put here the output of the following commands:\n", 0) + 1
	except ValueError:
		out.error("install log section not found in template. skipping.")
	else:
		command_lines = 1

		while t[i + command_lines] != "\n":
			command_lines += 1

		current_line = i + command_lines

		log = x.getElementsByTagName("log")[0]
		while command_lines:
			current_line = i + command_lines
			for child in log.childNodes:
				try:
					if child.getAttribute("name") == t[current_line].strip("\n"):
						t.insert(current_line + 1, str(child.childNodes[0].nodeValue).replace("\t", ""))
						t[current_line] = "# " + t[current_line]
				except Exception:
					pass
			command_lines -= 1

	return t

