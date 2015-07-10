# -*- coding: utf-8 -*-
#
# xml log output
#

import re
import xml.dom.minidom

from mtui.utils import *


class XMLOutput(object):

    def __init__(self):
        impl = xml.dom.minidom.getDOMImplementation()

        self.output = impl.createDocument(None, 'update', None)
        self.update = self.output.documentElement

    def add_header(self, metadata):
        self.update.setAttribute('md5', str(metadata.md5))
        for (type, id) in metadata.patches.items():
            self.update.setAttribute(type, id)

        self.update.setAttribute('swamp', metadata.swampid)
        self.update.setAttribute('packager', metadata.packager)
        self.update.setAttribute('category', metadata.category)

    def add_target(self, target):
        hostnode = self.output.createElement('host')
        hostnode.setAttribute('hostname', target.hostname)
        hostnode.setAttribute('system', target.system)
        self.update.appendChild(hostnode)

        self.add_package_state(hostnode, target, 'before')
        self.add_package_state(hostnode, target, 'after')

        self.add_log(hostnode, target)

    def add_package_state(self, parent, target, state):
        node = self.output.createElement(state)
        for package in target.packages:
            self.add_package(node, package, str(getattr(target.packages[package], state)))
        parent.appendChild(node)

        return node

    def add_package(self, parent, name, version):
        package = self.output.createElement('package')
        package.setAttribute('name', name)
        package.setAttribute('version', version)
        parent.appendChild(package)

    def add_log(self, parent, target):
        node = self.output.createElement('log')

        for (command, stdout, stderr, exitcode, runtime) in target.log:
            command = command.decode('ascii', 'replace').encode('ascii', 'replace')
            stdout = stdout.decode('ascii', 'replace').encode('ascii', 'replace')
            stderr = stderr.decode('ascii', 'replace').encode('ascii', 'replace')
            self.add_command(node, command, '%s\n%s' % (stdout, stderr), exitcode, runtime)

        parent.appendChild(node)

    def add_command(self, parent, command, output, exitcode, runtime):
        node = self.output.createElement('command')
        node.setAttribute('name', command)
        node.setAttribute('return', str(exitcode))
        node.setAttribute('time', str(runtime))
        text = self.output.createTextNode(output)
        node.appendChild(text)
        parent.appendChild(node)

    def pretty(self):
        return filter_ansi(self.output.toprettyxml())


