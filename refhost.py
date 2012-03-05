#!/usr/bin/python
# -*- coding: utf-8 -*-

import re
import logging

from xml.dom import minidom

out = logging.getLogger('mtui')

class Attributes(object):

    tags = {'products':['sled', 'sles', 'opensuse', 'rt', 'studio', 'smt', 'slms', 'vmware'],
             'archs':['i386', 'x86_64', 'ppc', 'ppc64', 's390', 's390x', 'ia64'],
             'major':['9', '10', '11', '12'],
             'minor':['sp1', 'sp2', 'sp3', 'sp4', '1', '2', '3', '4'],
             'addons':['webyast', 'webyast12'],
             'virtual':['xen', 'xenu', 'xen0', 'host', 'guest', 'kvm'],
             'tags':['kernel', 'ltss']
            }

    def __init__(self):
        self.product = ""
        self.arch = ""
        self.addons = []
        self.major = None
        self.minor = None
        self.release = None
        self.kernel = False
        self.ltss = False
        self.virtual = {'mode':'', 'hypervisor':''}

    def __str__(self):
        version = ''
        kernel = ''
        ltss = ''
        addons = ''

        if self.major:
            version = self.major
        if self.minor:
            version = version + self.minor
        if version.isdigit():
            version = "%s.%s" % (self.major, self.minor)
        if self.release:
            version = version + self.release
        if self.kernel:
            kernel = 'kernel'
        if self.ltss:
            ltss = 'ltss'

        for addon in self.addons:
            addons = " ".join([addons, addon])

        rep = ' '.join([self.product, version, self.arch, kernel, ltss, self.virtual['mode'], self.virtual['hypervisor'], addons])
        return ' '.join(rep.split())


class Refhost(object):

    def __init__(self, hostmap, location=None, attributes=Attributes()):
        if location is None:
            self.location = 'default'
        else:
            self.location = location

        self.attributes = attributes
        self.data = minidom.parse(hostmap)

        try:
            self.location_element = filter(self.is_location_element, self.data.getElementsByTagName('location'))[0]
        except:
            out.warning('location "%s" not found in %s. falling back to "default"' % (location, hostmap))
            self.location_element = filter(self.is_default_location_element, self.data.getElementsByTagName('location'))[0]

    def extract_name(self, element):
        return element.getAttribute('name')

    def search(self, attributes=None):
        if attributes is not None:
            self.attributes = attributes

        hosts = map(self.extract_name, filter(self.check_attributes, self.location_element.getElementsByTagName('host')))

        if not hosts:
            default_location = filter(self.is_default_location_element, self.data.getElementsByTagName('location'))[0]
            hosts = map(self.extract_name, filter(self.check_attributes, default_location.getElementsByTagName('host')))

        return hosts

    def check_attributes(self, element):
        try:
            if self.attributes.arch:
                assert(element.getAttribute('arch') == self.attributes.arch)

            if self.attributes.product:
                assert(element.getElementsByTagName('product')[0].getAttribute('name') == self.attributes.product)

            for addon in self.attributes.addons:
                assert(addon in map(self.extract_name, element.getElementsByTagName('addon')))

            for node in element.getElementsByTagName('addon'):
                if node.getAttribute('property') != 'weak':
                    assert(self.extract_name(node) in self.attributes.addons)

            node = element.getElementsByTagName('product')[0]
            major = node.getElementsByTagName('major')[0].firstChild.data
            try:
                minor = node.getElementsByTagName('minor')[0].firstChild.data
            except:
                minor = None
            try:
                release = node.getElementsByTagName('release')[0].firstChild.data
            except:
                release = None

            if self.attributes.major:
                assert(self.attributes.major == major)
            if self.attributes.minor:
                assert(self.attributes.minor == minor)
            if self.attributes.release:
                assert(self.attributes.release == release)

            try:
                node = element.getElementsByTagName('kernel')[0]
                if self.attributes.kernel:
                    assert(node.firstChild.data == 'true')
                else:
                    assert(node.getAttribute('property') == 'weak' or node.firstChild.data == 'false')
            except IndexError:
                assert(self.attributes.kernel is False)

            try:
                node = element.getElementsByTagName('ltss')[0]
                prop = node.getAttribute('property')
                if self.attributes.ltss:
                    assert(node.firstChild.data == 'true')
                else:
                    assert(node.getAttribute('property') == 'weak' or node.firstChild.data == 'false')
            except IndexError:
                assert(self.attributes.ltss is False)

            try:
                node = element.getElementsByTagName('virtual')[0]
                prop = node.getAttribute('property')
                mode = node.getAttribute('mode')
                if self.attributes.virtual['mode']:
                    assert(self.attributes.virtual['mode'] == mode)
                if self.attributes.virtual['hypervisor']:
                    assert(self.attributes.virtual['hypervisor'] == node.firstChild.data)
                if not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor']:
                    assert(node.getAttribute('property') == 'weak')
            except IndexError:
                assert(not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor'])

        except AssertionError:
            return False

        return True

    def is_location_element(self, element):
        if element.getAttribute('name') == self.location:
            return True
        else:
            return False

    def is_default_location_element(self, element):
        if element.getAttribute('name') == 'default':
            return True
        else:
            return False

    def get_host_attributes(self, hostname):
        attributes = Attributes()

        nodes = filter(lambda x: x.getAttribute('name') == hostname, self.location_element.getElementsByTagName('host'))

        for node in nodes:
            for element in node.getElementsByTagName('product'):
                attributes.product = element.getAttribute('name')
                for major in element.getElementsByTagName('major'):
                    attributes.major = major.firstChild.data
                for minor in element.getElementsByTagName('minor'):
                    attributes.minor = minor.firstChild.data
                for release in element.getElementsByTagName('release'):
                    attributes.release = release.firstChild.data

            attributes.arch = node.getAttribute('arch')

            for addons in node.getElementsByTagName('addon'):
                attributes.addons.append(addons.getAttribute('name'))

            for element in node.getElementsByTagName('kernel'):
                if element.firstChild.data == 'true':
                    attributes.kernel = True

            for element in node.getElementsByTagName('ltss'):
                if element.firstChild.data == 'true':
                    attributes.ltss = True

            for element in node.getElementsByTagName('virtual'):
                attributes.virtual = {'mode':element.getAttribute('mode'), 'hypervisor':element.firstChild.data}

        return attributes

    def set_attributes_from_system(self, system):
        attributes = Attributes()

        addons = []
        tags = system.split('-')
        name = tags[0]
        attributes.arch = tags[1]
        if len(tags) == 3 and tags[2] == 'kernel':
            attributes.kernel = True

        tags = name.split('_')
        name = tags[0]
        if len(tags) > 1:
            addons = tags[1:]

        match = re.search('(sl\D*)(.+)', name)
        if match:
            attributes.product = match.group(1)
            if attributes.product == 'sl':
                attributes.product = 'opensuse'

            version = match.group(2)
            match = re.search('(\d+)\.?(.*)', version, re.IGNORECASE)
            if match:
                attributes.major = match.group(1)
                attributes.minor = match.group(2)

        for addon in addons:
            if addon == 'XEN0':
                attributes.virtual.update({'mode':'host', 'hypervisor':'xen'})
            if addon == 'XENU':
                attributes.virtual.update({'mode':'guest', 'hypervisor':'xen'})
            if addon == 'ltss':
                attributes.ltss = True
            if addon in attributes.tags['products']:
                attributes.product = addon
            if addon in attributes.tags['addons']:
                attributes.addon.append(addon)

        self.attributes = attributes
