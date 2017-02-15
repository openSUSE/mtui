# -*- coding: utf-8 -*-
#
# xml log output
#

import xml.dom.minidom

from mtui.utils import filter_ansi


class XMLOutput(object):

    def __init__(self):
        impl = xml.dom.minidom.getDOMImplementation()

        self.output = impl.createDocument(None, 'update', None)
        self.update = self.output.documentElement

    def add_header(self, metadata):
        for (type, id) in metadata.patches.items():
            self.update.setAttribute(type, id)

        self.update.setAttribute('packager', metadata.packager)
        self.update.setAttribute('category', metadata.category)

    def add_target(self, target):
        node = self.output.createElement('host')
        node.setAttribute('hostname', target.hostname)
        node.setAttribute('system', target.system)
        self.update.appendChild(node)

        self.add_package_state(node, target, 'before')
        self.add_package_state(node, target, 'after')

        self.add_log(node, target)

    def add_package_state(self, parent, target, state):
        node = self.output.createElement(state)
        for package in target.packages:
            self.add_package(
                node, package, str(
                    getattr(
                        target.packages[package], state)))
        parent.appendChild(node)

        return node

    def add_package(self, parent, name, version):
        node = self.output.createElement('package')
        node.setAttribute('name', name)
        node.setAttribute('version', version)
        parent.appendChild(node)

    def add_log(self, parent, target):
        node = self.output.createElement('log')

        for (command, stdout, stderr, exitcode, runtime) in target.log:
            command = command.decode(
                'ascii',
                'replace').encode(
                'ascii',
                'replace')
            stdout = stdout.decode(
                'ascii',
                'replace').encode(
                'ascii',
                'replace')
            stderr = stderr.decode(
                'ascii',
                'replace').encode(
                'ascii',
                'replace')
            self.add_command(
                node, command, '{!s}\n{!s}'.format(
                    stdout, stderr), exitcode, runtime)

        parent.appendChild(node)

    def add_command(self, parent, command, output, exitcode, runtime):
        node = self.output.createElement('command')
        node.setAttribute('name', command)
        node.setAttribute('return', str(exitcode))
        node.setAttribute('time', str(runtime))
        node.appendChild(self.output.createTextNode(output))
        parent.appendChild(node)

    def pretty(self):
        return filter_ansi(self.output.toprettyxml())
