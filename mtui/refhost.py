# -*- coding: utf-8 -*-
#
# managing and parsing of the refhosts.yml file
#
import re
import os
import time
import errno
from urllib.request import urlopen

from mtui.xdg import save_cache_path
from mtui.utils import atomic_write_file
from mtui import messages

from traceback import format_exc

import ruamel.yaml as yaml
import copy


class Attributes(object):
    """
    Host attributes.
    This class has two purposes: to set the the attributes of a refhost and to be used as object for searching refhosts
    """

    def __init__(self):
        # scalar attributes
        self.minimal = None
        self.arch = ''

        # list attributes
        self.addons = []

        # table attributes
        self.product = {}
        self.kernel = {}
        self.ltss = {}
        self.virtual = {}

    def __str__(self):
        """
        Human readable output of the current attributes
        """

        product = ''
        if 'name' in self.product:
            product = self.product['name']
            if 'version' in self.product:
                product += ' '+str(self.product['version']['major'])
                if 'minor' in self.product['version']:
                    # If it's numbers, then a.b, if it's strings then ab
                    if(isinstance(self.product['version']['minor'], int)):
                        product += '.'

                    product += str(self.product['version']['minor'])

        kernel = ''
        if 'enabled' in self.kernel and self.kernel['enabled']:
            kernel = 'kernel'

        ltss = ''
        if 'enabled' in self.ltss and self.ltss['enabled']:
            ltss = 'ltss'

        minimal = ''
        if self.minimal:
            minimal = 'minimal'
        addons = []
        for addon in sorted(self.addons, key=lambda addon: addon['name']):
            serialization = addon['name']
            if 'version' in addon and 'major' in addon['version']:
                serialization += ' '+str(addon['version']['major'])+'.'
                if 'minor' in addon['version']:
                    serialization += str(addon['version']['minor'])

            addons.append(serialization)

        virtual = ' '
        if 'mode' in self.virtual:
            virtual = self.virtual['mode']
        if 'hypervisor' in self.virtual:
            virtual += ' '+self.virtual['hypervisor']

        representation = ' '.join([
            product,
            self.arch,
            kernel,
            ltss,
            minimal,
            virtual,
            ' '.join(addons)
        ])
        # remove the double spaces
        representation = re.sub(r"\s+", " ", representation.strip())
        # make ' ' to be ''. Just to pass the tests :-)
        return re.sub(r"^\s+$", "", representation)

    # Used in the tests
    def __bool__(self):
        """
        :returns: True if attributes have been set on this object
        """
        return bool(str(self))

    @staticmethod
    def from_testplatform(testplatform, log):
        """
        Create a list of Attribute objects based on a testplaform string

        :returns: list of Attributes
        """
        attributes_list = []
        # typical string:
        # base=sles(major=11,minor=sp4);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=sp4)
        attribute = Attributes()
        arch_list = []
        for pattern in testplatform.split(';'):
            try:
                property_name, content = pattern.split('=', 1)
            except ValueError:
                log.error('error when parsing line "{!s}"'.format(testplatform))
                continue
            # special cases: arch, virtual
            # *- arch because is a list so it will create a list of several attributes
            # *- virtual because it contains specific data
            # --
            # The rest of elements contains a version
            if property_name == "arch":
                capture = re.match(r"\[(.*)\]", content)
                code_evaluation = "','".join(capture.group(1).split(','))
                arch_list = eval("['{0}']".format(code_evaluation))
            elif property_name == "virtual":
                capture = re.match(r"\((.*)\)", content)
                virtual_property = {}
                for element in capture.group(1).split(','):
                    [key, value] = element.split('=')
                    virtual_property[key] = value
                setattr(attribute, property_name, virtual_property)
            elif property_name == "tags":
                capture = re.match(r"\((.*)\)", content)
                setattr(attribute, capture.group(1), {'enabled': True})
            else:
                complex_property = {'version': {}}
                capture = re.match(r"(.*)\((.*)\)", content)
                complex_property['name'] = capture.group(1)

                for element in capture.group(2).split(','):
                    [key, value] = element.split('=')
                    if value != '':
                        # We want number as numbers not as strings
                        try:
                            complex_property['version'][key] = int(value)
                        except Exception:
                            complex_property['version'][key] = value

                if property_name == "base":
                    attribute.product = complex_property
                elif property_name == "addon":
                    attribute.addons.append(complex_property)
                else:
                    setattr(attribute, property_name, complex_property)

        for arch in arch_list:
            attribute_copy = copy.copy(attribute)  # no need for deepcopy
            attribute_copy.arch = arch
            attributes_list.append(attribute_copy)

        return attributes_list


class Refhosts(object):
    _default_location = 'default'

    def __init__(self, hostmap, log, location=None):
        """
        load refhosts.yml file and pass it to the xml parser

        Keyword arguments:
        hostmap   -- path to the refhosts.yml file
        location  -- location to load hosts from (nuremberg, beijing...)
        attributes-- predefined search attributes

        """
        self.log = log

        # default refhosts location is 'default' which is basically fallback
        if location is None:
            self.location = self._default_location
        else:
            self.location = location

        self._parse_refhosts(hostmap)

    def _parse_refhosts(self, hostmap):
        try:
            with open(hostmap) as file:
                self.data = yaml.safe_load(file)

        except Exception as error:
            # nothing to do for us if we can't load the hosts
            self.log.error('failed to parse refhosts.yml: {!s}'.format(error))
            raise

    def search(self, attributes=None):
        """
        Return hosts matching `attributes`

        :return: [str] - Every element is the name of a host
        """

        results = []

        for attribute in attributes:
            host = []
            host = [
                candidate['name'] for candidate in self.data[
                    self.location] if self.is_candidate_match(
                    candidate, attribute)]

            if host == [] and self.location != self._default_location:
                host = [
                    candidate['name']
                    for candidate in self.data[self._default_location]
                    if self.is_candidate_match(candidate, attribute)]

            results += host

        return results

    def is_candidate_match(self, candidate, attribute):
        """
        Checks if the attributes contains all the info requested in
        candidate The candidate is a dictionary that represents a host in the
        refhosts

        :returns: True if the attributes contains the same candidate data.
        False otherwise
        """
        for key in vars(attribute):
            if getattr(attribute, key):
                if key not in candidate:
                    return False
                elif key == 'addons':
                    if not self._includes_addons_list(candidate[key], getattr(attribute, key)):
                        return False
                elif (isinstance(candidate[key], str) or
                      isinstance(candidate[key], int) or
                      isinstance(candidate[key], bool)):  # scalar options. Options that are non iterable
                    if getattr(attribute, key) != candidate[key]:
                        return False
                else:
                    if not self._includes_simple_attributes(candidate[key], getattr(attribute, key)):
                        return False

        return True

    def _includes_simple_attributes(self, candidate, attribute):
        """
        Helper function for is_candidate_match
        Checks if all candidate data is present in the element.

        :returns: True candidate data is present in the element. Returns
        False otherwise
        """

        for k in attribute:
            if k not in candidate:
                return False
            elif k == 'version':
                if not self._includes_simple_attributes(
                        candidate['version'],
                        attribute['version']):
                    return False
            elif attribute[k] != candidate[k]:
                return False

        return True

    def _includes_addons_list(self, candidate_addons, element_addons):
        """
        Helper function for is_candidate_match.
        Checks if all the addons are present in the element addons


        :returns: True when all addons data is present in the elements.
        False otherwise
        """

        element_addons_map = {addon['name']: addon for addon in element_addons}
        candidate_addons_map = {addon['name']: addon
                                for addon in candidate_addons}

        for addon in element_addons_map:
            if addon not in candidate_addons_map:
                return False
            else:
                if not self._includes_simple_attributes(
                        candidate_addons_map[addon],
                        element_addons_map[addon]):
                    return False
        return True

    def _location_hosts(self, location):
        """
        :returns: List of <host> elements for `location`

        :type  location: string
        """
        return self.data[location]

    def check_location_sanity(self, location):
        """
        :raises: L{messages.InvalidLocationError}
        """
        if location not in self.data:
            raise messages.InvalidLocationError(location, self.get_locations())

    def get_locations(self):
        """
        Return available locations

        :returns: set of strings
        """

        return set(self.data.keys())

    def get_host_attributes(self, hostname):
        """
        return attributes object for the hostname

        Keyword arguments:
        hostname -- host to return attributes for
        """

        attributes = Attributes()

        nodes = [e for e in self._location_hosts(
                self.location) if e['name'] == hostname]

        if nodes == [] and self.location != self._default_location:
            nodes = [e for e in self._location_hosts(
                    self._default_location) if e['name'] == hostname]

        # technically this iterates over all found host elements.
        # but since we just return one attribute object, we choose the first
        # one for now and return
        for node in nodes:
            if 'addons' in node:
                attributes.addons = node['addons']
            if 'product' in node:
                attributes.product = node['product']
            if 'arch' in node:
                attributes.arch = node['arch']
            if 'kernel' in node:
                attributes.kernel = node['kernel']
            if 'ltss' in node:
                attributes.ltss = node['ltss']
            if 'minimal' in node:
                attributes.minimal = node['minimal']
            if 'virtual' in node:
                attributes.virtual = node['virtual']

            return attributes

    def get_host_systemname(self, hostname):
        """
        assemble a host systemname from a given hostname

        Keyword arguments:
        hostname -- host to return the systemname for

        """
        attributes = self.get_host_attributes(hostname)

        addons = "_".join([ad['name'] for ad in attributes.addons])

        if attributes and "manager-client" in addons:
            system = "{0}{1}".format(
                attributes.product['name'],
                attributes.product['version']['major'])
            if 'minor' in attributes.product['version']:
                system += "{0}".format(attributes.product['version'])
            system += "-manager-client-{0}".format(attributes.arch)
        elif addons:

            system = '{!s}{!s}'.format(
                attributes.product['name'],
                attributes.product['version']['major'])

            if 'minor' in attributes.product['version']:
                system += '{!s}'.format(attributes.product['version']['minor'])

            # Unfortuanetly names of moudules are often too long
            system += "_{!s}-{!s}".format("module", attributes.arch)
        else:

            system = '{!s}{!s}'.format(
                attributes.product['name'],
                attributes.product['version']['major'])

            if 'minor' in attributes.product['version']:
                system += "{!s}".format(attributes.product['version']['minor'])

            system += "-{!s}".format(attributes.arch)

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
        self,
        time_now_getter,
        statter,
        urlopener,
        file_writer,
        cache_path,
        refhosts_factory=Refhosts
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
        if self._is_https_cache_refresh_needed(
                path, config.refhosts_https_expiration):
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
    save_cache_path('refhosts.yml'))
