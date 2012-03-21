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
             'addons':['webyast', 'webyast', 'webyast11', 'webyast12', 'sdk', 'hae'],
             'virtual':['xen', 'xenu', 'xen0', 'host', 'guest', 'kvm'],
             'tags':['kernel', 'ltss']
            }

    def __init__(self):
        self.product = ""
        self.archs = []
        self.addons = {}
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
                version = '%s.%s' % (self.major, self.minor)
        if self.release:
            version = version + self.release
        if self.kernel:
            kernel = 'kernel'
        if self.ltss:
            ltss = 'ltss'

        for addon in self.addons:
            addons = ' '.join([addons, addon])

            major = self.addons[addon]['major']
            minor = self.addons[addon]['minor']
            if major or minor:
                addons = ' '.join([addons, '%s.%s' % (major, minor)])


        archs = ' '.join(set(self.archs))

        rep = ' '.join([self.product, version, archs, kernel, ltss, self.virtual['mode'], self.virtual['hypervisor'], addons])
        return ' '.join(rep.split())


class Refhost(object):

    def __init__(self, hostmap, location=None, attributes=Attributes()):
        if location is None:
            self.location = 'default'
        else:
            self.location = location

        self.attributes = attributes
        try:
            self.data = minidom.parse(hostmap)
        except Exception, error:
            out.error('failed to parse refhosts.xml: %s' % error)
            return

    def extract_name(self, element):
        return element.getAttribute('name')

    def search(self, attributes=None):
        if attributes is not None:
            self.attributes = attributes

        try:
            location_element = filter(self.is_location_element, self.data.getElementsByTagName('location'))[0]
            hosts = map(self.extract_name, filter(self.check_attributes, location_element.getElementsByTagName('host')))
            assert(hosts)
        except (AssertionError, IndexError):
            location_element = filter(self.is_default_location_element, self.data.getElementsByTagName('location'))[0]
            hosts = map(self.extract_name, filter(self.check_attributes, location_element.getElementsByTagName('host')))

        return hosts

    def check_attributes(self, element):
        try:
            hostname = element.getAttribute('name')
            if self.attributes.archs:
                assert(element.getAttribute('arch') in self.attributes.archs)

            if self.attributes.product:
                assert(element.getElementsByTagName('product')[0].getAttribute('name') == self.attributes.product)

            for addon in self.attributes.addons:
                assert(addon in map(self.extract_name, element.getElementsByTagName('addon')))

            for node in element.getElementsByTagName('addon'):
                name = self.extract_name(node)
                if node.getAttribute('property') != 'weak':
                    assert(name in self.attributes.addons)
                if name in ['sdk', 'hae']:
                    continue
                try:
                    major = node.getElementsByTagName('major')[0].firstChild.data
                except:
                    major = ''
                try:
                    minor = node.getElementsByTagName('minor')[0].firstChild.data
                except:
                    minor = ''
                try:
                    assert(self.attributes.addons[name]['major'] == major)
                except KeyError:
                    pass
                try:
                    assert(self.attributes.addons[name]['minor'] == minor)
                except KeyError:
                    pass

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
            except IndexError:
                assert(not self.attributes.virtual['mode'])
                assert(not self.attributes.virtual['hypervisor'])
            else:
                prop = node.getAttribute('property')
                mode = node.getAttribute('mode')
                if self.attributes.virtual['mode']:
                    assert(self.attributes.virtual['mode'] == mode)
                if self.attributes.virtual['hypervisor']:
                    assert(self.attributes.virtual['hypervisor'] == node.firstChild.data)
                if not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor']:
                    assert(node.getAttribute('property') == 'weak')

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

        try:
            location_element = filter(self.is_location_element, self.data.getElementsByTagName('location'))[0]
            nodes = filter(lambda x: x.getAttribute('name') == hostname, location_element.getElementsByTagName('host'))
            assert(nodes)
        except (AssertionError, IndexError):
            location_element = filter(self.is_default_location_element, self.data.getElementsByTagName('location'))[0]
            nodes = filter(lambda x: x.getAttribute('name') == hostname, location_element.getElementsByTagName('host'))

        for node in nodes:
            for element in node.getElementsByTagName('product'):
                attributes.product = element.getAttribute('name')
                for major in element.getElementsByTagName('major'):
                    attributes.major = major.firstChild.data
                for minor in element.getElementsByTagName('minor'):
                    attributes.minor = minor.firstChild.data
                for release in element.getElementsByTagName('release'):
                    attributes.release = release.firstChild.data

            attributes.archs.append(node.getAttribute('arch'))

            for addons in node.getElementsByTagName('addon'):
                major = ''
                minor = ''
                try:
                    major = addons.getElementsByTagName('major')[0].firstChild.data
                except:
                    pass
                try:
                    minor = addons.getElementsByTagName('minor')[0].firstChild.data
                except:
                    pass
                attributes.addons.update({addons.getAttribute('name'):{'major':major, 'minor':minor}})

            for element in node.getElementsByTagName('kernel'):
                if element.firstChild.data == 'true':
                    attributes.kernel = True

            for element in node.getElementsByTagName('ltss'):
                if element.firstChild.data == 'true':
                    attributes.ltss = True

            for element in node.getElementsByTagName('virtual'):
                attributes.virtual = {'mode':element.getAttribute('mode'), 'hypervisor':element.firstChild.data}

        return attributes

    def get_host_systemname(self, hostname):
        attributes = self.get_host_attributes(hostname)
        addons = "_".join(set(attributes.addons.keys()).difference(['sdk', 'hae']))
        if addons:
            system = '%s%s%s_%s-%s' % (attributes.product, attributes.major, attributes.minor, addons, attributes.archs[0])
        else:
            system = '%s%s%s-%s' % (attributes.product, attributes.major, attributes.minor, attributes.archs[0])

        return system

    def set_attributes_from_system(self, system):
        attributes = Attributes()

        addons = []
        tags = system.split('-')
        name = tags[0]
        attributes.archs.append(tags[1])
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

    def set_attributes_from_testplatform(self, testplatform):
        requests = {}
        attributes = Attributes()

        patterns = testplatform.split(';')
        for pattern in patterns:
            name, content = pattern.split('=', 1)
            matches = re.findall('(\w+)\(([^\)]+)\)', content)
            for match in matches:
                    subpattern = match[0]
                    parameters = match[1]
                    for parameter in parameters.split(','):
                        key, value = parameter.split('=', 1)
                        try:
                            requests[name][subpattern].update({key:value})
                        except KeyError, error:
                            if error.message == name:
                                requests[name] = {subpattern:{key:value}}
                            else:
                                requests[name][subpattern] = {key:value}
            if name == 'arch':
                match = re.search('\[(.*)\]', content)
                if match:
                    requests[name] = match.group(1).split(',')
            if name == 'tags':
                match = re.search('\((.*)\)', content)
                if match:
                    requests[name] = match.group(1).split(',')

        attributes.archs = requests['arch']
        attributes.product = requests['base'].keys()[0]
        attributes.major = requests['base'][attributes.product]['major']
        attributes.minor = requests['base'][attributes.product]['minor']

        try:
            tags = requests['tags']
        except KeyError:
            tags = []

        for tag in tags:
            if tag == 'xen':
                attributes.virtual.update({'hypervisor':'xen'})
            if tag == 'kernel':
                attributes.kernel = True
            if tag == 'ltss':
                attributes.ltss = True

        try:
            for addon in requests['addon']:
                try:
                    major = requests['addon'][addon]['major']
                except:
                    major = ''
                try:
                    minor = requests['addon'][addon]['minor']
                except:
                    minor = ''
                attributes.addons.update({addon:{'major':major,'minor':minor}})
        except KeyError:
            pass

        self.attributes = attributes

