#!/usr/bin/env python
# -*- coding: utf-8 -*-

import sys
import os
import getopt

from export import *

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

	try:
		template = xml_to_template(t, x)
	except Exception:
		print "error: could not convert XML"
		return

	if output:
		try:
			with open(output, 'w') as f:
				f.write("".join(l.encode("utf-8") for l in template))
		except Exception as error:
			print "failed to write %s: %s" % (output, str(error))
		else:
			print "wrote template to %s" % output
	else:
		print "".join(l.encode("utf-8") for l in template)

		
if __name__ == "__main__":
	main()
