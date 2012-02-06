#!/usr/bin/python
# -*- coding: utf-8 -*-

import re
import logging

from xml.dom import minidom

out = logging.getLogger('mtui')

class Attributes(object):

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

class Refhost(object):

    def __init__(self, hostmap, location=None, attributes=Attributes()):
        if location is None:
            self.location = 'default'
        else:
            self.location = location

        self.attributes = attributes
        self.data = minidom.parse(hostmap)

        try:
            self.location_element = filter(self.get_location_element, self.data.getElementsByTagName('location'))[0]
        except:
            out.warning('location "%s" not found in %s. falling back to "default"' % (location, hostmap))
            self.location_element = filter(self.get_default_location_element, self.data.getElementsByTagName('location'))[0]

    def extract_name(self, element):
        return element.getAttribute('name')

    def search(self, attributes=None):
        if attributes is not None:
            self.attributes = attributes

        hosts = map(self.extract_name, filter(self.check_attributes, self.location_element.getElementsByTagName('host')))

        if not hosts:
            default_location = filter(self.get_default_location_element, self.data.getElementsByTagName('location'))[0]
            hosts = map(self.extract_name, filter(self.check_attributes, default_location.getElementsByTagName('host')))

        return hosts

    def check_attributes(self, element):
        if self.attributes.arch and element.getAttribute('arch') != self.attributes.arch:
            return False

        if self.attributes.product and element.getElementsByTagName('product')[0].getAttribute('name') != self.attributes.product:
            return False

        for addon in self.attributes.addons:
            if addon not in map(self.extract_name, element.getElementsByTagName('addon')):
                return False

        for node in element.getElementsByTagName('addon'):
            prop = node.getAttribute('property')
            if self.extract_name(node) not in self.attributes.addons and not prop == 'weak':
                return False

        try:
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

            if self.attributes.major and self.attributes.major != major:
                return False
            if self.attributes.minor and self.attributes.minor != minor:
                return False
            if self.attributes.release and self.attributes.release != release:
                return False
        except:
            return False

        try:
            node = element.getElementsByTagName('kernel')[0]
            prop = node.getAttribute('property')
            if self.attributes.kernel and not node.firstChild.data == 'true':
                return False
            if not self.attributes.kernel and node.firstChild.data == 'true' and not prop == 'weak':
                return False
        except:
            if self.attributes.kernel:
                return False

        try:
            node = element.getElementsByTagName('ltss')[0]
            prop = node.getAttribute('property')
            if self.attributes.ltss and not node.firstChild.data == 'true':
                return False
            if not self.attributes.ltss and node.firstChild.data == 'true' and not prop == 'weak':
                return False
        except:
            if self.attributes.ltss:
                return False

        try:
            node = element.getElementsByTagName('virtual')[0]
            prop = node.getAttribute('property')
            mode = node.getAttribute('mode')
            if self.attributes.virtual['mode'] and self.attributes.virtual['mode'] != mode:
                return False
            if self.attributes.virtual['hypervisor'] and self.attributes.virtual['hypervisor'] != node.firstChild.data:
                return False
            if not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor'] and not prop == 'weak':
                return False
        except:
            if self.attributes.virtual['mode'] or self.attributes.virtual['hypervisor']:
                return False

        return True

    def get_location_element(self, element):
        if element.getAttribute('name') == self.location:
            return True
        else:
            return False

    def get_default_location_element(self, element):
        if element.getAttribute('name') == 'default':
            return True
        else:
            return False

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
            if addon in ['rt', 'studio', 'studio12', 'smt', 'slms', 'slms12']:
                attributes.product = addon
            if addon in ['webyast', 'webyast12']:
                attributes.addon.append(addon)

        self.attributes = attributes

