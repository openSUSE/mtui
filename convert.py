#!/usr/bin/env python
# -*- coding: utf-8 -*-

import sys
import os
import re
import getopt
import xml.dom.minidom

def xml_to_template(templatefile, xmlfile):

	with open(templatefile, 'r') as f:
		t = f.readlines()

	x = xml.dom.minidom.parse(xmlfile)

	for host in x.getElementsByTagName("host"):
		line = "%s (reference host: %s)\n" % (host.getAttribute("system"), host.getAttribute("hostname"))
		try:
			i = t.index(line) + 4
		except:
			print "host section %s not found" % host.getAttribute("hostname")
			continue
		for state in ["before", "after"]:
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
			i += 1

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
			except:
				pass
		command_lines -= 1

	return t

def usage():
	print
	print "XML to Template Converter"
	print "=" * 35
	print
	print sys.argv[0], "{-t|--template file} {-x|--xml file} [-o|--output file]"
	print

	sys.exit(0)

def main():
	x = ""
	t = ""
	output = ""

	try:
		opts, args = getopt.getopt(sys.argv[1:], "ht:x:o:", ["help", "template=", "xml=", "output="])
	except getopt.GetoptError as error:
		print str(error)
		usage()

	for parameter, argument in opts:
		if parameter in ('-h', '--help'):
			usage()
		elif parameter in ('-t', '--template'):
			t = argument
		elif parameter in ('-x', '--xml'):
			x = argument
		elif parameter in ('-o', '--output'):
			output = argument
		else:
			usage()
		
	if not os.path.isfile(t) or not os.path.isfile(x):
		usage()

	template = xml_to_template(t, x)

	if output:
		try:
			with open(output, 'w') as f:
				f.write("".join(template))
		except Exception as error:
			print "failed to write %s: %s" % (output, str(error))
		else:
			print "wrote template to %s" % output
	else:
		print "".join(template)

		
if __name__ == "__main__":
	main()
