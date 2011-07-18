#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import logging
import codecs
import xml.dom.minidom

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
	except Exception as ex:
		print repr(ex)
		out.error("could not parse XML data")
		raise AttributeError("XML")

	for host in x.getElementsByTagName("host"):
		hostname = host.getAttribute("hostname")
		systemtype = host.getAttribute("system")

		line = "%s (reference host: %s)\n" % (systemtype, hostname)
		try:
			i = t.index(line)
		except ValueError:
			out.debug("host section %s not found, searching system" % hostname)
			line = "%s (reference host: ? )\n" % systemtype
			try:
				i = t.index(line)
				t[i] = "%s (reference host: %s)\n" % (systemtype, hostname)
			except ValueError:
				out.debug("system section %s not found, creating new one" % systemtype)
				line = "Test results by product-arch:\n"

				i = t.index(line) + 2
				t.insert(i, "\n")
				i += 1
				t.insert(i, "%s (reference host: %s)\n" % (systemtype, hostname))
				i += 1
				t.insert(i, "--------------\n")
				i += 1
				t.insert(i, "Update\n")
				i += 1
				t.insert(i, "      before:\n")
				i += 1
				t.insert(i, "      after:\n")
				i += 1
				t.insert(i, "\n")
				i += 1
				t.insert(i, "      => PASSED/FAILED\n")
				i += 1
				t.insert(i, "\n")
				i += 1
				t.insert(i, "      comment: (none)\n")
				i += 1
				t.insert(i, "\n")

	for host in x.getElementsByTagName("host"):
		hostname = host.getAttribute("hostname")
		systemtype = host.getAttribute("system")

		line = "%s (reference host: %s)\n" % (systemtype, hostname)
		try:
			i = t.index(line)
		except ValueError:
			out.warning("host section %s not found" % hostname)
			continue
		for state in ["before", "after"]:
			i = t.index("      %s:\n" % state, i) + 1
			for package in host.getElementsByTagName(state):
				for child in package.childNodes:
					try:
						name = child.getAttribute("name")
						version = child.getAttribute("version")
						if version != "0":
							t.insert(i, "\t%s-%s\n" % (name, version))
						else:
							t.insert(i, "\tpackage %s is not installed\n" % name)
						i += 1
					except Exception:
						pass

	try:
		generic_testcases_pos = t.index("generic test cases:\n") + 4
	except ValueError:
		out.debug('old "generic tests cases:" section not found in template')
	try:
		generic_testcases_pos = t.index("regression tests:\n") + 4
	except ValueError:
		out.debug('new "regression tests:" section not found in template')

	try:
		generic_testcases_pos
	except NameError:
		out.error("regression testing section not found in template. skipping.")
	else:
		for host in x.getElementsByTagName("host"):
			hostname = host.getAttribute("hostname")
			systemtype = host.getAttribute("system")
			while t[generic_testcases_pos].strip("\n"):
				generic_testcases_pos += 1
			generic_testcases_pos += 1

			t.insert(generic_testcases_pos, "=== %s ===\n" % hostname)
			generic_testcases_pos += 1

			log = host.getElementsByTagName("log")[0]
			for child in log.childNodes:
				try:
					name = child.getAttribute("name")
					exitcode = child.getAttribute("return")

					if "scripts/compare/compare_" in name:
						scriptname = os.path.basename(name.split(" ")[0])

						if exitcode == "0":
							result = "SUCCEEDED"
						elif exitcode == "1":
							result = "FAILED"
						else:
							result = "INTERNAL ERROR"

						t.insert(generic_testcases_pos, "%s - %s\n" % (scriptname, result))
						generic_testcases_pos += 1

				except Exception:
					pass

			t.insert(generic_testcases_pos, "\n")

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

