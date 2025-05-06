#
# managing and parsing of the refhosts.yml file
#
import copy
import errno
import os
import re
import time
from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import final
from urllib.request import urlopen

from ruamel.yaml import YAML

from . import messages
from .utils import atomic_write_file
from .xdg import save_cache_path

logger = getLogger("mtui.refhost")


@final
class Attributes:
    """
    Host attributes.
    This class has two purposes: to set the the attributes of a refhost and
    to be used as object for searching refhosts
    """

    def __init__(self) -> None:
        self.arch = ""
        self.addons = []
        self.product = {}

    def __str__(self) -> str:
        """
        Human readable output of the current attributes
        """

        product: str = ""
        if "name" in self.product:
            product = self.product["name"]
            if "version" in self.product:
                product += " " + str(self.product["version"]["major"])
                if "minor" in self.product["version"]:
                    # If it's numbers, then a.b, if it's strings then ab
                    if isinstance(self.product["version"]["minor"], int):
                        product += "."

                    product += str(self.product["version"]["minor"])

        addons: list[str] = []
        for addon in sorted(self.addons, key=lambda addon: addon["name"]):
            serialization = addon["name"]
            if "version" in addon and "major" in addon["version"]:
                serialization += " " + str(addon["version"]["major"]) + "."
                if "minor" in addon["version"]:
                    serialization += str(addon["version"]["minor"])

            addons.append(serialization)

        representation = " ".join([product, self.arch, " ".join(addons)])

        # remove the double spaces
        representation = re.sub(r"\s+", " ", representation.strip())
        # make ' ' to be ''. Just to pass the tests :-)
        return re.sub(r"^\s+$", "", representation)

    # Used in the tests
    def __bool__(self) -> bool:
        """
        :returns: True if attributes have been set on this object
        """
        return bool(str(self))

    @staticmethod
    def from_testplatform(testplatform) -> list["Attributes"]:
        """
        Create a list of Attribute objects based on a testplaform string

        :returns: list of Attributes
        """
        attributes_list = []
        # typical string:
        # base=sles(major=11,minor=sp4);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=sp4)
        # base=sles(major=11,minor=sp4);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=)
        # base=sles(major=11,minor=sp4);arch=[i386,s390x,x86_64];addon=sdk(major=11)
        attribute = Attributes()
        arch_list = []
        for pattern in testplatform.split(";"):
            try:
                property_name, content = pattern.split("=", 1)
            except ValueError:
                logger.error('error when parsing line "{!s}"'.format(testplatform))
                continue
            # special case: arch
            # *- arch because is a list so it will create a list of several attributes
            # --
            # The rest of elements contains a version
            if property_name == "arch":
                capture = re.match(r"\[(.*)\]", content)
                code_evaluation = "','".join(capture.group(1).split(","))
                arch_list = eval("['{0}']".format(code_evaluation))
            elif property_name == "tags":
                capture = re.match(r"\((.*)\)", content)
                setattr(attribute, capture.group(1), {"enabled": True})
            else:
                complex_property = {"version": {}}
                capture = re.match(r"(.*)\((.*)\)", content)
                complex_property["name"] = capture.group(1)

                for element in capture.group(2).split(","):
                    [key, value] = element.split("=")
                    # Note: When the minor is '' then it's used to search for unset values
                    # We want number as numbers not as strings
                    try:
                        complex_property["version"][key] = int(value)
                    except ValueError:
                        complex_property["version"][key] = value

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


class Refhosts:
    _default_location = "default"

    def __init__(self, hostmap: Path, location: str | None = None) -> None:
        """
        load refhosts.yml file and pass it to the xml parser

        Keyword arguments:
        hostmap   -- path to the refhosts.yml file
        location  -- location to load hosts from (nuremberg, beijing...)
        attributes-- predefined search attributes

        """

        # default refhosts location is 'default' which is basically fallback
        if location is None:
            self.location = self._default_location
        else:
            self.location = location

        self._parse_refhosts(hostmap)

    def _parse_refhosts(self, hostmap: Path) -> None:
        try:
            with hostmap.open() as f:
                self.data = YAML(typ="safe").load(f)

        except Exception as error:
            # nothing to do for us if we can't load the hosts
            logger.error("failed to parse refhosts.yml: %s", error)
            raise

    def search(self, attributes) -> list[str]:
        """
        Return hosts matching `attributes`

        :return: [str] - Every element is the name of a host
        """

        results: list[str] = []

        for attribute in attributes:
            host = []
            host = [
                candidate["name"]
                for candidate in self.data[self.location]
                if self.is_candidate_match(candidate, attribute)
            ]

            if host == [] and self.location != self._default_location:
                host = [
                    candidate["name"]
                    for candidate in self.data[self._default_location]
                    if self.is_candidate_match(candidate, attribute)
                ]

            results += host

        return results

    def is_candidate_match(self, candidate, attribute) -> bool:
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
                elif key == "addons":
                    if not self._includes_addons_list(
                        candidate[key], getattr(attribute, key)
                    ):
                        return False
                elif (
                    isinstance(candidate[key], str)
                    or isinstance(candidate[key], int)
                    or isinstance(candidate[key], bool)
                ):  # scalar options. Options that are non iterable
                    if getattr(attribute, key) != candidate[key]:
                        return False
                else:
                    if not self._includes_simple_attributes(
                        candidate[key], getattr(attribute, key)
                    ):
                        return False

        return True

    def _includes_simple_attributes(self, candidate, attribute) -> bool:
        """
        Helper function for is_candidate_match
        Checks if all candidate data is present in the element.

        Example:
        base=sles(major=12,minor=sp1);arch=[s390x,x86_64];addon=Web-Scripting(major=12,minor=)
        this should search for any entries with web-scripting that has an unset minor version

        base=sles(major=12,minor=sp3);arch=[s390x,x86_64];addon=Web-Scripting(major=12,minor=foo)
        this should search for any entries with web-scripting minor=foo

        base=sles(major=12,minor=sp3);arch=[s390x,x86_64];addon=Web-Scripting(major=12)
        this should search for any entries with web-scripting major=12 no matter which minor

        Important: It's always one line per version.

        :returns: True candidate data is present in the element. Returns
        False otherwise
        """

        for k in attribute:
            if k not in candidate:
                return False
            elif k == "version":
                if not self._includes_version(
                    candidate["version"], attribute["version"]
                ):
                    return False
            elif attribute[k] != candidate[k]:
                return False

        return True

    def _includes_version(self, candidate, element) -> bool:
        """
        Helper function for _is_candidate_match.
        """
        if "minor" in element and element["minor"] != "":
            if "minor" not in candidate or element["minor"] != candidate["minor"]:
                return False
        elif "minor" in element and element["minor"] == "":
            if "minor" in candidate:
                return False

        # major is mandatory
        if element["major"] != candidate["major"]:
            return False

        return True

    def _includes_addons_list(self, candidate_addons, element_addons) -> bool:
        """
        Helper function for is_candidate_match.
        Checks if all the addons are present in the element addons

        :returns: True when all addons data is present in the elements.
        False otherwise
        """

        element_addons_map = {addon["name"]: addon for addon in element_addons}
        candidate_addons_map = {addon["name"]: addon for addon in candidate_addons}

        for addon in element_addons_map:
            if addon not in candidate_addons_map:
                return False
            else:
                if not self._includes_simple_attributes(
                    candidate_addons_map[addon], element_addons_map[addon]
                ):
                    return False
        return True

    def _location_hosts(self, location: str):
        """
        :returns: List of <host> elements for `location`

        :type  location: string
        """
        return self.data[location]

    def check_location_sanity(self, location) -> None:
        """
        :raises: L{messages.InvalidLocationError}
        """
        if location not in self.data:
            raise messages.InvalidLocationError(location, self.get_locations())

    def get_locations(self) -> set[str]:
        """
        Return available locations

        :returns: set of strings
        """

        return set(self.data.keys())


class RefhostsResolveFailed(RuntimeError):
    pass


class _RefhostsFactory:
    # FIXME: split resolvers into separate classes
    # should help with the ammount of injected dependencies in each one
    # of the classes

    # _stat = None
    """
    :type _stat: callable :: FilePath -> IO L{posix.stat_result}
    """

    # _urlopen = None
    """
    :type urlopen: callable :: URI -> IO file-like
    """
    # _time_now = None
    """
    :type time_now: callable :: IO float
    :param time_now_getter: returns unix time
    """

    # _write_file = None
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
        refhosts_factory: type[Refhosts] = Refhosts,
    ) -> None:
        self._time_now = time_now_getter
        self._stat = statter
        self._urlopen = urlopener
        self._write_file = file_writer

        self.refhosts_cache_path = cache_path
        self.refhosts_factory = refhosts_factory

    def __call__(self, config):
        for resolver in [x.strip() for x in config.refhosts_resolvers.split(",")]:
            try:
                return self._resolve_one(resolver, config)
            except BaseException:
                logger.warning("Refhosts: resolver {0} failed".format(resolver))
                logger.debug(format_exc())

        raise RefhostsResolveFailed()

    def _resolve_one(self, name, config):
        try:
            resolver = getattr(self, "resolve_{0}".format(name))
        except AttributeError:
            logger.warning("Refhosts: invalid resolver: {0}".format(name))
            raise
        else:
            return resolver(config)

    def refresh_https_cache_if_needed(self, path: Path, config) -> None:
        if self._is_https_cache_refresh_needed(path, config.refhosts_https_expiration):
            self.refresh_https_cache(path, config.refhosts_https_uri)

    def _is_https_cache_refresh_needed(self, path, expiration) -> bool:
        try:
            statinfo = self._stat(path)
        except OSError as e:
            if e.errno == errno.ENOENT:
                return True
            else:
                raise

        return self._time_now() - statinfo.st_mtime > expiration

    def refresh_https_cache(self, path, uri) -> None:
        self._write_file(self._urlopen(uri).read(), path)

    def resolve_https(self, config) -> Refhosts:
        f = self.refhosts_cache_path
        self.refresh_https_cache_if_needed(f, config)

        return self.refhosts_factory(f, config.location)

    def resolve_path(self, config) -> Refhosts:
        return self.refhosts_factory(config.refhosts_path, config.location)


RefhostsFactory = _RefhostsFactory(
    time.time, os.stat, urlopen, atomic_write_file, save_cache_path("refhosts.yml")
)
