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
		line = "%s (reference host: %s)\n" % (host.getAttribute("system"), host.getAttribute("hostname"))
		try:
			i = t.index(line)
		except:
			print "host section %s not found" % host.getAttribute("hostname")
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
					except:
						pass

	i = t.index("put here the output of the following commands:\n", 0) + 1
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
			except:
				pass
		command_lines -= 1

	return t

