# -*- coding: utf-8 -*-
#
# managing and parsing of the refhosts.xml file
#

import re
import operator
import os
import time
import errno
from mtui.five import urlopen

from xml.dom import minidom
from mtui.xdg import save_cache_path
from mtui.utils import atomic_write_file
from mtui.utils import flatten
from mtui import messages

from traceback import format_exc


class Attributes(object):

    """Host attributes which get loaded from the xml or serve as search criteria

    any tag specified here gets loaded as valid search tag in prompt.py
    adding tags needs only to be done here

    """

    tags = {
        'products': [
            'sled',
            'sles',
            'opensuse',
            'studio',
            'slms',
            'sles4vmware',
            'manager',
            'rhel',
            'sle'],
        'archs': [
            'i386',
            'x86_64',
            'ppc',
            'ppc64',
            'ppc64le',
            's390',
            's390x',
            'ia64',
            'iseries'],
        'major': [
            '9',
            '10',
            '11',
            '12',
            '5',
            '6'],
        'minor': [
            'sp1',
            'sp2',
            'sp3',
            'sp4',
            '1',
            '2',
            '3',
            '4'],
        'addons': [
            'webyast',
            'webyast11',
            'webyast12',
            'sdk',
            'hae',
            'studiorunner',
            'smt',
            'manager-client',
            'rt',
            'we'],
        'virtual': [
            'xen',
            'xenu',
            'xen0',
            'host',
            'guest',
            'kvm',
            'vmware',
            'lpar'],
        'tags': [
            'kernel',
            'ltss',
            'minimal']}

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
        self.virtual = {'mode': '', 'hypervisor': ''}

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

        for addon in sorted(self.addons.keys()):
            # add addon name followed by addon version to the string
            addons = ' '.join([addons, addon])

            major = self.addons[addon].get('major', '')
            minor = self.addons[addon].get('minor', '')

            if major or minor:
                addons = ' '.join([addons, '%s.%s' % (major, minor)])

        archs = ' '.join(sorted(set(self.archs)))

        rep = ' '.join([self.product,
                        version,
                        archs,
                        kernel,
                        ltss,
                        minimal,
                        self.virtual['mode'],
                        self.virtual['hypervisor'],
                        addons])
        return ' '.join(rep.split())

    def __bool__(self):
        """
        :returns: True if attributes have been set on this object
        """
        return bool(str(self))

    def __nonzero__(self):
        """python-2.x compat"""
        return self.__bool__()

    @classmethod
    def from_search_hosts_query(cls, q):
        attrs = Attributes()

        for _tag in q.split(' '):
            tag = _tag.lower()
            match = re.search('(\d+)\.(\d+)', tag)
            if match:
                attrs.major = match.group(1)
                attrs.minor = match.group(2)
            if tag in attrs.tags['major']:
                attrs.major = tag
            if tag in attrs.tags['minor']:
                attrs.minor = tag

            if tag in attrs.tags['products']:
                attrs.product = tag
            if tag in attrs.tags['archs']:
                attrs.archs.append(tag)
            if tag in attrs.tags['addons']:
                attrs.addons.update({tag: dict()})

            if tag in ('kernel', 'ltss', 'minimal'):
                setattr(attrs, tag, True)

            if tag in ('!kernel', '!ltss', '!minimal'):
                setattr(attrs, tag[1:], False)

            if tag in ('xen', 'kvm', 'vmware'):
                attrs.virtual.update(hypervisor=tag)

            if tag == 'xenu':
                attrs.virtual.update(mode='guest', hypervisor='xen')
            if tag == 'xen0':
                attrs.virtual.update(mode='host', hypervisor='xen')

            if tag == 'host':
                attrs.virtual.update(mode='host')
            if tag == 'guest':
                attrs.virtual.update(mode='guest')

        return attrs

    @classmethod
    def from_testplatform(cls, testplatform, log):
        """
        Create a attribute object based on a testplatform string

        Keyword arguments:
        testplatform -- testplatform string to return the attributes for

        """

        # testreport string example:
        # base=sled(major=10,minor=sp4);arch=[i386,x86_64]

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

            # add all required architectures to the dict
            if name == 'arch':
                match = re.search('\[(.*)\]', content)
                if match:
                    requests[name] = match.group(1).split(',')
                continue

            # add all required tags to the dict (like kernel or ltss)
            # add all required virtual descriptors to the dict (like "mode" or
            # "hypervisor")
            if name in ('tags', 'virtual'):
                match = re.search('\((.*)\)', content)
                if match:
                    requests[name] = match.group(1).split(',')
                continue

            scope = requests.setdefault(name, dict())
            # get all subpatterns and parameters, like subpattern = 'sled'
            # parameters = major=10,minor=sp4
            matches = re.findall('([\w_-]+)\(([^\)]+)\)', content)
            for match in matches:
                subpattern = match[0]
                parameters = match[1]
                # split parameter assignments in key and value, like
                # key = major, value = 10
                scope.setdefault(subpattern, dict()) \
                    .update([p.split('=', 1) for p in parameters.split(',')])

        # assign the findings to the attributes object
        attributes.archs = sorted(requests['arch'])
        # currently, just one base product is supported
        attributes.product = list(requests['base'].keys())[0]
        base = requests['base'][attributes.product]
        if 'major' in base:
            attributes.major = base['major']
        if 'minor' in base:
            attributes.minor = base['minor']

        tags = requests.get('tags', [])

        # if we found tags in the testplatform string, add them to the
        # attributes
        for tag in tags:
            if tag in ('vmware', 'xen'):
                attributes.virtual.update(hypervisor=tag)
            if tag in ('kernel', 'ltss', 'minimal'):
                setattr(attributes, tag, True)

        # add adons to the attributes
        addons = requests.get('addon', dict())
        for addon, aversion in addons.items():
            # if no version is required, leave them empty
            major = aversion.get('major', '')
            minor = aversion.get('minor', '')
            attributes.addons.setdefault(
                addon,
                dict()).update(
                major=major,
                minor=minor)

        # add virtual descriptors to the attributes (may overwrite xen tag)
        for descriptor in requests.get('virtual', []):
            for parameter in descriptor.split(','):
                key, value = parameter.split('=', 1)
                attributes.virtual[key] = value

        return attributes


class Refhosts(object):
    _default_location = 'default'

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
            self.location = self._default_location
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
        """
        Return hosts matching `attributes`

        :return: [str]
        """

        # if no attributes were set, search by the default attributes
        if attributes is not None:
            self.attributes = attributes

        archs = self.attributes.archs
        if not archs:
            archs = attributes.tags['archs']

        results = []
        # workaround for multiple-arch-searches since the default location
        # isn't used if the overlay location returns at least one host.
        # example: searching for i386 and s390x doesn't search for s390x
        # in the default location if a host is returned for i386 from the
        # overlay location.
        for arch in archs:
            self.attributes.archs = [arch]

            hosts = list(
                map(self.extract_name,
                    filter(self.check_attributes, self._location_hosts(
                        self.location))))

            if hosts == [] and self.location != self._default_location:
                try:
                    hosts = list(map(
                        self.extract_name, filter(
                            self.check_attributes, self._location_hosts(self._default_location)
                            )))
                except messages.InvalidLocationError:
                    pass

            results += hosts

        self.attributes.archs = archs
        return results

    def _location_hosts(self, location):
        """
        :returns: List of <host> elements for `location`

        :type  location: string
        """
        return flatten([
            x.getElementsByTagName('host')
            for x in self._locations(location)
        ])

    def _locations(self, location):
        """
        :returns: <location> elements for `location`
        :raises: L{messages.InvalidLocationError}

        :type  location: string
        """
        xs = list(
            filter(
                lambda e: operator.eq(
                    e.getAttribute('name'),
                    location),
                self.data.getElementsByTagName('location')))

        if xs == []:
            raise messages.InvalidLocationError(
                location, self.get_locations()
            )

        return xs

    def check_location_sanity(self, location):
        """
        :raises: L{messages.InvalidLocationError}
        """
        self._locations(location)

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
                product = element.getElementsByTagName(
                    'product')[0].getAttribute('name')
                if self.attributes.product == "sle":
                    product = product[0:-1]
                assert(product == self.attributes.product)

            for addon in self.attributes.addons:
                # each addon in the search attributes is available on this host
                assert(
                    addon in map(
                        self.extract_name,
                        element.getElementsByTagName('addon')))

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
                    major = node.getElementsByTagName(
                        'major')[0].firstChild.data
                except:
                    major = ''
                try:
                    minor = node.getElementsByTagName(
                        'minor')[0].firstChild.data
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
                release = node.getElementsByTagName(
                    'release')[0].firstChild.data
            except:
                release = None

            # product versions need to match
            assert(self.attributes.major == ('' if major is None else major))
            assert(self.attributes.minor == ('' if minor is None else minor))
            if self.attributes.release:
                assert(self.attributes.release == release)

            try:
                # kernel element found on the host. make sure we are searching for
                # a kernel host, or the kernel host must not be exclusive.
                node = element.getElementsByTagName('kernel')[0]
                if self.attributes.kernel:
                    assert(node.firstChild.data == 'true')
                elif self.attributes.kernel is False:
                    assert(
                        node.getAttribute('property') == 'weak'
                        or node.firstChild.data == 'false')
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
                    assert(
                        node.getAttribute('property') == 'weak'
                        or node.firstChild.data == 'false')
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
                    assert(
                        node.getAttribute('property') == 'weak'
                        or node.firstChild.data == 'false')
            except IndexError:
                # minimal element not found for the host. make sure we do not
                # require the host to be a minimal host
                assert(not self.attributes.minimal)

            try:
                node = element.getElementsByTagName('virtual')[0]
            except IndexError:
                # no virtual element found for the host. make sure we don't search
                # for virtualized hosts or hipervisors.
                assert(
                    (not self.attributes.virtual['mode'])
                    or self.attributes.virtual['mode'] == "none")
                assert(not self.attributes.virtual['hypervisor'])
            else:
                # if a virtual element was found, make sure it matches our search
                # criteria (mode/hypervisor) or is not exclusive
                prop = node.getAttribute('property')
                mode = node.getAttribute('mode')
                if self.attributes.virtual['mode']:
                    assert(self.attributes.virtual['mode'] == mode)
                if self.attributes.virtual['hypervisor']:
                    assert(
                        self.attributes.virtual['hypervisor'] ==
                        node.firstChild.data)
                if not self.attributes.virtual['mode'] and not self.attributes.virtual['hypervisor']:
                    assert(node.getAttribute('property') == 'weak')

        except AssertionError:
            # catch all failed assertions and discard this host for
            # the search
            return False

        return True

    def get_locations(self):
        """
        Return available locations

        :returns: set of strings
        """

        return set([
            e.getAttribute('name')
            for e in self.data.getElementsByTagName('location')
        ])

    def get_host_attributes(self, hostname):
        """return attributes object for the hostname

        Keyword arguments:
        hostname -- host to return attributes for

        """

        attributes = Attributes()

        nodes = filter(
            lambda e: operator.eq(
                e.getAttribute('name'), hostname), self._location_hosts(
                self.location))

        if nodes == [] and self.location != self._default_location:
            nodes = filter(
                lambda e: operator.eq(
                    e.getAttribute('name'), hostname), self._location_hosts(
                    self._default_location))

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
                    major = addons.getElementsByTagName(
                        'major')[0].firstChild.data
                except:
                    pass
                try:
                    minor = addons.getElementsByTagName(
                        'minor')[0].firstChild.data
                except:
                    pass
                attributes.addons.update(
                    {addons.getAttribute('name'):
                     {'major': major, 'minor': minor}})

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
                attributes.virtual = {
                    'mode': element.getAttribute('mode'),
                    'hypervisor': element.firstChild.data}

            return attributes

    def get_host_systemname(self, hostname):
        """assemble a host systemname from a given hostname

        Keyword arguments:
        hostname -- host to return the systemname for

        """

        attributes = self.get_host_attributes(hostname)

        # don't add addon names to the systemname for now since this would
        # be incompatible to our legacy systemnames
        addons = "_".join(
            set(attributes.addons.keys()).difference(['sdk', 'hae']))
        # if addons:
        #    system = '%s%s%s_%s-%s' % (attributes.product, attributes.major, attributes.minor, addons, attributes.archs[0])
        # else:
        #    system = '%s%s%s-%s' % (attributes.product, attributes.major, attributes.minor, attributes.archs[0])
        if attributes and "manager-client" in addons:
            system = '%s%s%s-manager-client-%s' % (attributes.product,
                                                   attributes.major,
                                                   attributes.minor,
                                                   attributes.archs[0])
        elif attributes:
            system = '%s%s%s-%s' % (attributes.product,
                                    attributes.major,
                                    attributes.minor,
                                    attributes.archs[0])
        else:
            system = 'host_not_found'

        return system


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
        self, time_now_getter,  # +
        statter,                # |
        urlopener,              # |
        file_writer,            # +- these are needed only for https resolver
        cache_path, refhosts_factory=Refhosts
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
        if self._is_https_cache_refresh_needed(path, config.refhosts_https_expiration):
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
            config.refhosts_path, log, config.location
        )

RefhostsFactory = _RefhostsFactory(
    time.time,
    os.stat,
    urlopen,
    atomic_write_file,
    save_cache_path('refhosts.xml'))
