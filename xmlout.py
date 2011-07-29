#!/usr/bin/env python
# -*- coding: utf-8 -*-

import re
import xml.dom.minidom

class XMLOutput:
	def __init__(self, metadata=None):
		impl = xml.dom.minidom.getDOMImplementation()

		self.output = impl.createDocument(None, "update", None)
		self.update = self.output.documentElement

		if metadata is not None:
			self.add_header(metadata)

	def add_header(self, metadata):
		self.update.setAttribute("md5", metadata.md5)
		for type, id in metadata.patches.items():
			self.update.setAttribute(type, id)

		self.update.setAttribute("swamp", metadata.swampid)
		self.update.setAttribute("packager", metadata.packager)
		self.update.setAttribute("category", metadata.category)

	def add_target(self, target):
		hostnode = self.get_new_machine_node(target.hostname)
		self.set_attribute(hostnode, "system", target.system)

		statusnode = self.get_new_status_node(hostnode, "before")
		for package in target.packages:
			packagenode = self.get_new_package_node(statusnode, package, str(target.packages[package].before))

		statusnode = self.get_new_status_node(hostnode, "after")
		for package in target.packages:
			packagenode = self.get_new_package_node(statusnode, package, str(target.packages[package].after))

		lognode = self.get_new_log_node(hostnode)

		for command, stdout, stderr, exitcode, runtime in target.log:
			self.get_new_command_node(lognode, command, "%s\n%s" % (stdout, stderr), exitcode, runtime)

	def get_new_machine_node(self, hostname):
		self.machine = self.output.createElement("host")
		self.machine.setAttribute("hostname", hostname)
		self.update.appendChild(self.machine)

		return self.machine

	def get_new_status_node(self, parent, which):
		node = self.output.createElement(which)
		self.machine.appendChild(node)

		return node

	def get_new_package_node(self, parent, name, version):
		package = self.output.createElement("package")
		package.setAttribute("name", name)
		package.setAttribute("version", version)
		parent.appendChild(package)

	def get_new_log_node(self, parent):
		self.log = self.output.createElement("log")
		self.machine.appendChild(self.log)

		return self.log

	def get_new_command_node(self, parent, command, output, exitcode, runtime):
		node = self.output.createElement("command")
		node.setAttribute("name", command)
		node.setAttribute("return", str(exitcode))
		node.setAttribute("time", str(runtime))
		text = self.output.createTextNode(output)
		node.appendChild(text)
		parent.appendChild(node)

	def set_attribute(self, node, name, value):
		self.machine.setAttribute(name, value)

	def pretty(self):
		#return re.sub("\033\[[0-9;]+m", "", self.output.toprettyxml())
		return re.sub("\033", "", self.output.toprettyxml())
