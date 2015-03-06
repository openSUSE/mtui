# -*- coding: utf-8 -*-
#
# managing and parsing of the refhosts.xml file
#

import re
import os
import time
import errno
try:
    from urllib.request import urlopen
except ImportError:
    from urllib import urlopen

from xml.dom import minidom
from mtui.xdg import save_cache_path
from mtui.utils import atomic_write_file

from traceback import format_exc

class Attributes(object):
    """Host attributes which get loaded from the xml or serve as search criteria

    any tag specified here gets loaded as valid search tag in prompt.py
    adding tags needs only to be done here

    """

    tags = {'products':['sled', 'sles', 'opensuse', 'studio', 'slms', 'sles4vmware', 'manager', 'rhel', 'sle'],
             'archs':['i386', 'x86_64', 'ppc', 'ppc64', 'ppc64le', 's390', 's390x', 'ia64', 'iseries'],
             'major':['9', '10', '11', '12', '5', '6'],
             'minor':['sp1', 'sp2', 'sp3', 'sp4', '1', '2', '3', '4'],
             'addons':['webyast', 'webyast11', 'webyast12', 'sdk', 'hae', 'studiorunner', 'smt', 'manager-client', 'rt', 'we'],
             'virtual':['xen', 'xenu', 'xen0', 'host', 'guest', 'kvm', 'vmware'],
             'tags':['kernel', 'ltss', 'minimal']
            }

    def __init__(self):
        self.product = ""
        self.archs = []
        self.addons = {}
        self.major = None
        self.minor = None
        self.release = None
        # kernel, ltss and minimal can have 3 states: True (is kernel host)
        #                                    False (is not kernel host)
        #                                    None (not searched for)
        self.kernel = None
        self.ltss = None
        self.minimal = None
        # mode should have "host", "guest" or an empty value
        # hypervisor is arbitrary, but most likely xen or kvm
        self.virtual = {'mode':'', 'hypervisor':''}

    def __str__(self):
        """humand readable output of the current attributes"""

        version = ''
        kernel = ''
        ltss = ''
        minimal = ''
        addons = ''

        if self.major:
            version = self.major
        if self.minor:
            version = version + self.minor
            # if major and minor versions are digits only, it's most likely
            # a dotted version (i.e. 11.1)
            if version.isdigit():
                version = '%s.%s' % (self.major, self.minor)
        if self.release:
            version = version + self.release
        if self.kernel:
            kernel = 'kernel'
        if self.ltss:
            ltss = 'ltss'
        if self.minimal:
            minimal = 'minimal'

        for addon in self.addons:
            # add addon name followed by addon version to the string
            addons = ' '.join([addons, addon])

            try:
                major = self.addons[addon]['major']
            except KeyError:
                major = ""
            try:
                minor = self.addons[addon]['minor']
            except KeyError:
                minor = ""

            if major or minor:
                addons = ' '.join([addons, '%s.%s' % (major, minor)])


        archs = ' '.join(set(self.archs))

        rep = ' '.join([self.product, version, archs, kernel, ltss, minimal, self.virtual['mode'], self.virtual['hypervisor'], addons])
        return ' '.join(rep.split())

    def __bool__(self):
        """return if attributes have been set on this object"""

        if self.__str__():
            return True
        else:
            return False

    def __nonzero__(self):
        """python-2.x compat"""
        return self.__bool__()

    @classmethod
    def from_testplatform(cls, testplatform, log):
        """
        Create a attribute object based on a testplatform string

        Keyword arguments:
        testplatform -- testplatform string to return the attributes for

        """

        # testreport string example: base=sled(major=10,minor=sp4);arch=[i386,x86_64]

        requests = {}
        attributes = Attributes()
        attributes.kernel = False
        attributes.ltss = False
        attributes.minimal = False

        # split patterns to base, arch, addon, tags
        patterns = testplatform.split(';')
        for pattern in patterns:
            # get assignements for each pattern, like name = 'base',
            # content = sled(major=10,minor=sp4)
            try:
                name, content = pattern.split('=', 1)
            except ValueError:
                log.error('error when parsing line "%s"' % testplatform)
                continue

            # get all subpatterns and parameters, like subpattern = 'sled'
            # parameters = major=10,minor=sp4
            matches = re.findall('([\w_-]+)\(([^\)]+)\)', content)
            for match in matches:
                    subpattern = match[0]
                    parameters = match[1]
                    # split parameter assignments in key and value, like
                    # key = major, value = 10
                    for parameter in parameters.split(','):
                        key, value = parameter.split('=', 1)
                        try:
                            # add key and value to the name/subpattern dict
                            requests[name][subpattern].update({key:value})
                        except KeyError as error:
                            # if name or subpattern do not yet exist in the dict,
                            # create them. first make sure which one is missing:
                            # name or supbattern
                            if name == error.args[0]:
                                requests[name] = {subpattern:{key:value}}
                            else:
                                requests[name][subpattern] = {key:value}
            # add all required architectures to the dict
            if name == 'arch':
                match = re.search('\[(.*)\]', content)
                if match:
                    requests[name] = match.group(1).split(',')
            # add all required tags to the dict (like kernel or ltss)
            if name == 'tags':
                match = re.search('\((.*)\)', content)
                if match:
                    requests[name] = match.group(1).split(',')
            # add all required virtual descriptors to the dict (like "mode" or "hypervisor")
            if name == 'virtual':
                match = re.search('\((.*)\)', content)
                if match:
                    requests[name] = match.group(1).split(',')

        # assign the findings to the attributes object
        attributes.archs = requests['arch']
        # currently, just one base product is supported
        attributes.product = list(requests['base'].keys())[0]
        try:
            attributes.major = requests['base'][attributes.product]['major']
        except KeyError:
            pass
        try:
            attributes.minor = requests['base'][attributes.product]['minor']
        except KeyError:
            pass

        try:
            tags = requests['tags']
        except KeyError:
            tags = []

        # if we found tags in the testplatform string, add them to the attributes
        for tag in tags:
            if tag == 'vmware':
                attributes.virtual.update({'hypervisor':'vmware'})
            if tag == 'xen':
                attributes.virtual.update({'hypervisor':'xen'})
            if tag == 'kernel':
                attributes.kernel = True
            if tag == 'ltss':
                attributes.ltss = True
            if tag == 'minimal':
                attributes.minimal = True

        try:
            # add adons to the attributes
            for addon in requests['addon']:
                try:
                    # if no version is required, leave them empty
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

        try:
            # add virtual descriptors to the attributes (may overwrite xen tag)
            for descriptor in requests['virtual']:
                for parameter in descriptor.split(','):
                    key = parameter.split('=')[0]
                    value = parameter.split('=')[1]

                    attributes.virtual.update({key:value})
        except KeyError:
            pass

        return attributes

class Refhosts(object):
    def __init__(self, hostmap, log, location=None, attributes=Attributes()):
        """load refhosts.xml file and pass it to the xml parser

        Keyword arguments:
        hostmap   -- path to the refhosts.xml file
        location  -- location to load hosts from (nuremberg, beijing...)
        attributes-- predefined search attributes

        """
        self.log = log

        # default refhosts location is 'default' which is basically
        # nuremberg office
        if location is None:
            self.location = 'default'
        else:
            self.location = location

        # attributes of the last host searched for
        # at the end of the day, this may not really be useful and may
        # be removed somewhere in the future
        self.attributes = attributes
        self._parse_refhosts(hostmap)

    def _parse_refhosts(self, hostmap):
        try:
            self.data = minidom.parse(hostmap)
        except Exception as error:
            # nothing to do for us if we can't load the hosts
            self.log.error('failed to parse refhosts.xml: %s' % error)
            raise

    def extract_name(self, element):
        """extract value of the 'name' tag of the xml element

        Keyword arguments:
        element  -- XML Element

        """

        return element.getAttribute('name')

    def search(self, attributes=None):
        """search for hosts based on the attributes and return a list

        Keyword arguments:
        attributes -- attributes object to serach for

        """

        results = []
        # if no attributes were set, search by the default attributes
        if attributes is not None:
            self.attributes = attributes

        archs = self.attributes.archs
        if not archs:
            archs = attributes.tags['archs']

        # if we don't get a matching host on the location, search for the
        # same host in our default location

        # workaround for multiple-arch-searches since the default location
        # isn't used if the overlay location returns at least one host.
        # example: searching for i386 and s390x doesn't search for s390x
        # in the default location if a host is returned for i386 from the
        # overlay location.
        for arch in archs:
            self.attributes.archs = [arch]
            try:
                # get correct location element from a list of location elements
                location_element = list(filter(self.is_location_element, self.data.getElementsByTagName('location')))[0]
                # extract hostname on all hosts matching the filter criteria
                hosts = list(map(self.extract_name, filter(self.check_attributes, location_element.getElementsByTagName('host'))))
                assert(hosts)
            except (AssertionError, IndexError):
                # host not found in specified location, try again in default location
                location_element = list(filter(self.is_default_location_element, self.data.getElementsByTagName('location')))[0]
                hosts = list(map(self.extract_name, filter(self.check_attributes, location_element.getElementsByTagName('host'))))

            if hosts:
                results = results + hosts

        self.attributes.archs = archs
        return results

    def check_attributes(self, element):
        """check attributes of a specific host xml element

        assert each attribute match to be true,
        if an assertion is not met, return False

        Keyword arguments:
        element -- host xml element

        """

        try:
            if self.attributes.archs:
                # current host arch is in the searched arch list
                assert(element.getAttribute('arch') in self.attributes.archs)

            if self.attributes.product:
                # current host product is the searched product
                product = element.getElementsByTagName('product')[0].getAttribute('name')
                if self.attributes.product == "sle":
                    product = product[0:-1]
                assert(product == self.attributes.product)

            for addon in self.attributes.addons:
                # each addon in the search attributes is available on this host
                assert(addon in map(self.extract_name, element.getElementsByTagName('addon')))

            for node in element.getElementsByTagName('addon'):
                name = self.extract_name(node)
                if node.getAttribute('property') != 'weak':
                    # make sure that if an exclusive addon is installed on the host,
                    # it's as well in the searched attributes list.
                    assert(name in self.attributes.addons)
                if name in ['sdk', 'hae']:
                    # skip 'sdk' and 'hae' tags since they probably are installed
                    # on each host
                    continue
                try:
                    major = node.getElementsByTagName('major')[0].firstChild.data
                except:
                    major = ''
                try:
                    minor = node.getElementsByTagName('minor')[0].firstChild.data
                except:
                    minor = ''
                # check if the searched version numbers match the installed
                # addon versions. in case they do not match, an AssertionError
                # is thrown. in case they are irrelevant (not in the search
                # attributes), a KeyError is catched an ignored.
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

            # product versions need to match if they are specified
            if self.attributes.major:
                assert(self.attributes.major == major)
            if self.attributes.minor:
                assert(self.attributes.minor == minor)
            if self.attributes.release:
                assert(self.attributes.release == release)

            try:
                # kernel element found on the host. make sure we are searching for
                # a kernel host, or the kernel host must not be exclusive.
                node = element.getElementsByTagName('kernel')[0]
                if self.attributes.kernel:
                    assert(node.firstChild.data == 'true')
                elif self.attributes.kernel is False:
                    assert(node.getAttribute('property') == 'weak' or node.firstChild.data == 'false')
            except IndexError:
                # kernel element not found for the host. make sure we do not
                # require the host to be a kernel host
                assert(not self.attributes.kernel)

            try:
                # ltss element found on the host. make sure we are searching for
                # a ltss host, or the ltss host must not be exclusive.
                node = element.getElementsByTagName('ltss')[0]
                prop = node.getAttribute('property')
                if self.attributes.ltss:
                    assert(node.firstChild.data == 'true')
                elif self.attributes.ltss is False:
                    assert(node.getAttribute('property') == 'weak' or node.firstChild.data == 'false')
            except IndexError:
                # ltss element not found for the host. make sure we do not
                # require the host to be a ltss host
                assert(not self.attributes.ltss)

            try:
                # minimal element found on the host. make sure we are searching for
                # a minimal host, or the minimal host must not be exclusive.
                node = element.getElementsByTagName('minimal')[0]
                prop = node.getAttribute('property')
                if self.attributes.minimal:
                    assert(node.firstChild.data == 'true')
                elif self.attributes.minimal is False:
                    assert(node.getAttribute('property') == 'weak' or node.firstChild.data == 'false')
            except IndexError:
                # minimal element not found for the host. make sure we do not
                # require the host to be a minimal host
                assert(not self.attributes.minimal)

            try:
                node = element.getElementsByTagName('virtual')[0]
            except IndexError:
                # no virtual element found for the host. make sure we don't search
                # for virtualized hosts or hipervisors.
                assert((not self.attributes.virtual['mode']) or self.attributes.virtual['mode'] == "none")
                assert(not self.attributes.virtual['hypervisor'])
            else:
                # if a virtual element was found, make sure it matches our search
                # criteria (mode/hypervisor) or is not exclusive
                prop = node.getAttribute('property')
                mode = node.getAttribute('mode')
                if self.attributes.virtual['mode']:
                    assert(self.attributes.virtual['mode'] == mode)
                if self.attributes.virtual['hypervisor']:
                    assert(self.attributes.virtual['hypervisor'] == node.firstChild.data)
                if not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor']:
                    assert(node.getAttribute('property') == 'weak')

        except AssertionError:
            # catch all failed assertions and discard this host for
            # the search
            return False

        return True

    def get_locations(self):
        """return list of all available locations

        Keyword arguments:
        None

        """

        return [ element.getAttribute('name') for element in self.data.getElementsByTagName('location') ]

    def is_location_element(self, element):
        """check if the location element is the specified one

        Keyword arguments:
        element -- location xml element

        """

        if element.getAttribute('name') == self.location:
            return True
        else:
            return False

    def is_default_location_element(self, element):
        """check if the location element is the default location element

        Keyword arguments:
        element -- location xml element

        """

        if element.getAttribute('name') == 'default':
            return True
        else:
            return False

    def get_host_attributes(self, hostname):
        """return attributes object for the hostname

        Keyword arguments:
        hostname -- host to return attributes for

        """

        attributes = Attributes()

        try:
            # search for the hostname in all host elements below the specified location
            location_element = list(filter(self.is_location_element, self.data.getElementsByTagName('location')))[0]
            nodes = list([x for x in location_element.getElementsByTagName('host') if x.getAttribute('name') == hostname])
            assert(len(nodes))
        except (AssertionError, IndexError):
            # if no matchin hostnames are found, search again in the default location
            location_element = list(filter(self.is_default_location_element, self.data.getElementsByTagName('location')))[0]
            nodes = list([x for x in location_element.getElementsByTagName('host') if x.getAttribute('name') == hostname])

        # technically this iterates over all found host elements.
        # but since we just return one attribute object, we choose the first
        # one for now and return
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

            for element in node.getElementsByTagName('minimal'):
                if element.firstChild.data == 'true':
                    attributes.minimal = True

            for element in node.getElementsByTagName('virtual'):
                attributes.virtual = {'mode':element.getAttribute('mode'), 'hypervisor':element.firstChild.data}

            return attributes

    def get_host_systemname(self, hostname):
        """assemble a host systemname from a given hostname

        Keyword arguments:
        hostname -- host to return the systemname for

        """

        attributes = self.get_host_attributes(hostname)

        # don't add addon names to the systemname for now since this would
        # be incompatible to our legacy systemnames
        addons = "_".join(set(attributes.addons.keys()).difference(['sdk', 'hae']))
        #if addons:
        #    system = '%s%s%s_%s-%s' % (attributes.product, attributes.major, attributes.minor, addons, attributes.archs[0])
        #else:
        #    system = '%s%s%s-%s' % (attributes.product, attributes.major, attributes.minor, attributes.archs[0])
        if attributes and "manager-client" in addons:
            system = '%s%s%s-manager-client-%s' % (attributes.product, attributes.major, attributes.minor, attributes.archs[0])
        elif attributes:
            system = '%s%s%s-%s' % (attributes.product, attributes.major, attributes.minor, attributes.archs[0])
        else:
            system = 'host_not_found'

        return system

    def set_attributes_from_system(self, system):
        """create a attribute object based on a legacy systemname

        Keyword arguments:
        system -- systemname to return the attributes for

        """

        # systemname examples: sled10sp4-i386, sles11sp2_XEN0-i386-kernel

        attributes = Attributes()
        attributes.kernel = False
        attributes.ltss = False
        attributes.minimal = False

        addons = []
        # split by '-' since this looks like to be the delimiter
        tags = system.split('-')
        # name is the first one (sled10sp4)
        name = tags[0]
        # arch comes in second (i386)
        attributes.archs.append(tags[1])
        # if we have more than 2 tags (name and arch) we probably have a kernel
        # machine
        if len(tags) == 3 and tags[2] == 'kernel':
            attributes.kernel = True

        # ltss and XEN are preceded by '_'
        tags = name.split('_')
        # while the first element is still the name
        name = tags[0]
        # all other elements may he a hint to an addon
        if len(tags) > 1:
            addons = tags[1:]

        # check if it's a enterprise product or opensuse
        match = re.search('(sl\D*)(.+)', name)
        if match:
            attributes.product = match.group(1)
            if attributes.product == 'sl':
                attributes.product = 'opensuse'

            # split the version string from the name tag (sled10sp4)
            version = match.group(2)
            match = re.search('(\d+)\.?(.*)', version, re.IGNORECASE)
            if match:
                attributes.major = match.group(1)
                attributes.minor = match.group(2)

        # iterate over common addons or tags and add them to the attributes
        for addon in addons:
            if addon == 'XEN0':
                attributes.virtual.update({'mode':'host', 'hypervisor':'xen'})
            if addon == 'XENU':
                attributes.virtual.update({'mode':'guest', 'hypervisor':'xen'})
            if addon == 'ltss':
                attributes.ltss = True
            if addon == 'minimal':
                attributes.minimal = True
            if addon in attributes.tags['products']:
                attributes.product = addon
            if addon in attributes.tags['addons']:
                attributes.addons.update({addon:{}})

        # set the attributes of the current object. consider returning
        # the attributes object as well
        self.attributes = attributes

class RefhostsResolveFailed(RuntimeError):
    pass

class _RefhostsFactory(object):
    # FIXME: split resolvers into separate classes
    # should help with the ammount of injected dependencies in each one
    # of the classes

    _stat = None
    """
    :type _stat: callable :: FilePath -> IO L{posix.stat_result}
    """

    _urlopen = None
    """
    :type urlopen: callable :: URI -> IO file-like
    """
    _time_now = None
    """
    :type time_now: callable :: IO float
    :param time_now_getter: returns unix time
    """

    _write_file = None
    """
    :type _write_file: callable :: str -> FilePath -> IO ()
    :param _write_file: atomically writes data into given file path
    """

    def __init__(
      self
    , time_now_getter  # +
    , statter          # |
    , urlopener        # |
    , file_writer      # +- these are needed only for https resolver
    , cache_path
    , refhosts_factory=Refhosts
    ):
        self._time_now = time_now_getter
        self._stat = statter
        self._urlopen = urlopener
        self._write_file = file_writer

        self.refhosts_cache_path = cache_path
        self.refhosts_factory = refhosts_factory

    def __call__(self, config, log):
        for resolver in [x.strip()
        for x in config.refhosts_resolvers.split(",")]:
            try:
                return self._resolve_one(resolver, config, log)
            except:
                log.warning('Refhosts: resolver {0} failed'.format(
                    resolver))
                log.debug(format_exc())

        raise RefhostsResolveFailed()

    def _resolve_one(self, name, config, log):
        try:
            resolver = getattr(self, 'resolve_{0}'.format(name))
        except AttributeError:
            log.warning("Refhosts: invalid resolver: {0}".format(name))
            raise
        else:
            return resolver(config, log)

    def refresh_https_cache_if_needed(self, path, config):
        if self._is_https_cache_refresh_needed(path
        , config.refhosts_https_expiration):
            self.refresh_https_cache(path, config.refhosts_https_uri)

    def _is_https_cache_refresh_needed(self, path, expiration):
        try:
            statinfo = self._stat(path)
        except OSError as e:
            if e.errno == errno.ENOENT:
                return True
            else:
                raise

        return self._time_now() - statinfo.st_mtime > expiration

    def refresh_https_cache(self, path, uri):
        self._write_file(self._urlopen(uri).read(), path)

    def resolve_https(self, config, log):
        f = self.refhosts_cache_path
        self.refresh_https_cache_if_needed(f, config)

        return self.refhosts_factory(f, log, config.location)

    def resolve_path(self, config, log):
        return self.refhosts_factory(
              config.refhosts_path
            , log
            , config.location
        )

RefhostsFactory = _RefhostsFactory(
  time.time
, os.stat
, urlopen
, atomic_write_file
, save_cache_path('refhosts.xml')
)
