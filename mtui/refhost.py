"""Manages and parses `refhosts.yml` files.

This module provides typed dataclasses for the refhost schema
(:class:`Version`, :class:`Product`, :class:`Addon`, :class:`Host`,
:class:`Attributes`), the :class:`Refhosts` loader/search, and a
:class:`_RefhostsFactory` that resolves the YAML source via a registry
of :class:`Resolver` implementations (`https`, `path`).

The YAML schema:

.. code-block:: yaml

    <location_str>:
      - name: <hostname>
        arch: <arch>
        product:
          name: <name>
          version:
            major: <int|str>
            minor: <int|str>     # optional
        addons:                  # optional, list
          - name: <name>
            version:
              major: <int|str>
              minor: <int|str>   # optional

Top-level keys are user-supplied location names (``default``,
``nuremberg``, etc.); the ``default`` location is consulted as a
fallback when a per-location search yields no match.
"""

import copy
import errno
import os
import re
import time
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import Any, final
from urllib.request import urlopen

from ruamel.yaml import YAML

from . import messages
from .utils import atomic_write_file
from .xdg import save_cache_path

logger = getLogger("mtui.refhost")


# ---------------------------------------------------------------------------
# Typed schema
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Version:
    """A product or addon version.

    ``minor == ""`` is a sentinel used in *queries* (Attributes) to mean
    "match candidates that have no minor set". In *candidates* (Host),
    ``minor`` is ``None`` when the YAML omits the key.
    """

    major: int | str
    minor: int | str | None = None


@dataclass(frozen=True, slots=True)
class Product:
    """A base product (``sles``, ``SLE_RT``, …) with optional version."""

    name: str
    version: Version | None = None


@dataclass(frozen=True, slots=True)
class Addon:
    """An addon (module / extension) shipped on a refhost."""

    name: str
    version: Version | None = None


@dataclass(frozen=True, slots=True)
class Host:
    """One refhost row loaded from ``refhosts.yml``.

    Required fields (``name``, ``arch``, ``product``) come from the
    live ``refhosts-ng.yml`` schema; ``addons`` defaults to an empty
    list because hosts without addons omit the key entirely.
    """

    name: str
    arch: str
    product: Product
    addons: tuple[Addon, ...] = ()


@final
@dataclass(slots=True)
class Attributes:
    """Search query against a :class:`Refhosts` database.

    Each field is optional (empty/None sentinel); the matcher skips
    constraints on unset fields. This dataclass is *mutable* because
    :meth:`from_testplatform` builds it incrementally segment by segment.
    """

    arch: str = ""
    product: Product | None = None
    addons: list[Addon] = field(default_factory=list)

    def __str__(self) -> str:
        """Return a human-readable string representation of the query."""
        parts: list[str] = []

        if self.product is not None:
            parts.append(_format_named_version(self.product.name, self.product.version))

        if self.arch:
            parts.append(self.arch)

        # Addons are sorted alphabetically for stable output.
        parts.extend(
            _format_named_version(a.name, a.version)
            for a in sorted(self.addons, key=lambda a: a.name)
        )

        return " ".join(p for p in parts if p)

    def __repr__(self) -> str:
        return f"<Attributes: {self!s}>"

    def __bool__(self) -> bool:
        """Truthy when any constraint is set."""
        return bool(self.arch) or self.product is not None or bool(self.addons)

    @staticmethod
    def from_testplatform(testplatform: str) -> list["Attributes"]:
        """Parse a SMELT ``testplatform`` string into one Attributes per arch.

        Grammar: ``base=<name>(major=X,minor=Y);arch=[a,b,c];addon=<name>(...)``.
        Unknown segments are logged at ERROR and skipped.

        Example:

        .. code-block:: text

            base=sles(major=15,minor=5);arch=[x86_64,aarch64];addon=sdk(major=15,minor=5)

        """
        attribute = Attributes()
        arch_list: list[str] = []

        for pattern in testplatform.split(";"):
            try:
                property_name, content = pattern.split("=", 1)
            except ValueError:
                logger.error('error when parsing line "%s"', testplatform)
                continue

            if property_name == "arch":
                if capture := re.match(r"\[(.*)\]", content):
                    arch_list = [x.strip() for x in capture.group(1).split(",")]
            elif property_name == "base":
                if parsed := _parse_named_version(content):
                    attribute.product = Product(name=parsed[0], version=parsed[1])
            elif property_name == "addon":
                if parsed := _parse_named_version(content):
                    attribute.addons.append(Addon(name=parsed[0], version=parsed[1]))
            else:
                logger.error(
                    'unknown testplatform segment %r in "%s"',
                    property_name,
                    testplatform,
                )

        # Fan out one Attributes per arch; deepcopy because addons is
        # a mutable list of (frozen) Addon dataclasses.
        return [
            _attributes_with_arch(copy.deepcopy(attribute), arch) for arch in arch_list
        ]


def _attributes_with_arch(attr: Attributes, arch: str) -> Attributes:
    """Return ``attr`` with ``arch`` set (used by ``from_testplatform``)."""
    attr.arch = arch
    return attr


def _format_named_version(name: str, version: Version | None) -> str:
    """Render ``name`` + optional ``version`` like ``"sles 15.5"`` / ``"sles 12sp4"``."""
    if version is None:
        return name
    out = f"{name} {version.major}"
    if version.minor is not None and version.minor != "":
        # int minor uses a dot separator; string minor is concatenated.
        sep = "." if isinstance(version.minor, int) else ""
        out += f"{sep}{version.minor}"
    return out


def _parse_named_version(content: str) -> tuple[str, Version | None] | None:
    """Parse ``name(major=X,minor=Y)`` → ``(name, Version(...))``.

    Returns ``None`` if ``content`` does not match the grammar. ``minor``
    is preserved as the empty string ``""`` when the source has
    ``minor=`` (sentinel for "candidate has no minor"), as ``int`` when
    numeric, or as ``str`` otherwise.
    """
    capture = re.match(r"(.*)\((.*)\)", content)
    if not capture:
        return None
    name = capture.group(1)
    fields: dict[str, int | str] = {}
    for element in capture.group(2).split(","):
        try:
            key, value = element.split("=")
        except ValueError:
            continue
        try:
            fields[key] = int(value)
        except ValueError:
            fields[key] = value

    if "major" not in fields:
        return name, None
    version = Version(major=fields["major"], minor=fields.get("minor"))
    return name, version


def _version_from_dict(d: Any) -> Version | None:
    """Convert a YAML version dict to a :class:`Version` (or None).

    Raises :class:`TypeError` if ``d`` is non-None but not a mapping,
    or :class:`KeyError` if the required ``major`` key is missing.
    """
    if d is None:
        return None
    if not isinstance(d, dict):
        raise TypeError(f"expected version dict, got {type(d).__name__}")
    if "major" not in d:
        raise KeyError("major")
    return Version(major=d["major"], minor=d.get("minor"))


def _host_from_dict(d: Any) -> Host | None:
    """Convert one YAML row to a :class:`Host`.

    On schema failure (missing required field, wrong nesting type),
    logs at ERROR level and returns ``None`` so the loader can drop the
    bad row without aborting.
    """
    try:
        if not isinstance(d, dict):
            raise TypeError(f"expected mapping, got {type(d).__name__}")
        product_raw: Any = d["product"]
        if not isinstance(product_raw, dict):
            raise TypeError(
                f"product must be a mapping, got {type(product_raw).__name__}"
            )
        product = Product(
            name=product_raw["name"],
            version=_version_from_dict(product_raw.get("version")),
        )
        addons_raw: Any = d.get("addons") or []
        addons = tuple(
            Addon(name=a["name"], version=_version_from_dict(a.get("version")))
            for a in addons_raw
        )
        return Host(name=d["name"], arch=d["arch"], product=product, addons=addons)
    except (KeyError, TypeError) as e:
        logger.error("refhosts: dropping malformed host row %r: %s", d, e)
        return None


# ---------------------------------------------------------------------------
# Refhosts loader / search
# ---------------------------------------------------------------------------


class Refhosts:
    """Loads and searches ``refhosts.yml`` for hosts matching :class:`Attributes`."""

    _default_location = "default"

    def __init__(self, hostmap: Path, location: str | None = None) -> None:
        """Initialize the Refhosts object.

        Args:
            hostmap: Path to ``refhosts.yml``.
            location: Location key to search; defaults to ``"default"``.

        """
        self.location = location if location is not None else self._default_location
        self._parse_refhosts(hostmap)

    def _parse_refhosts(self, hostmap: Path) -> None:
        """Load ``hostmap`` and convert each row to a :class:`Host`.

        Malformed rows are logged at ERROR and dropped; YAML parse
        failures propagate.
        """
        try:
            with hostmap.open() as f:
                raw = YAML(typ="safe").load(f)
        except Exception:
            logger.error("failed to parse refhosts.yml")
            raise

        self.data: dict[str, list[Host]] = {}
        for loc, rows in (raw or {}).items():
            self.data[loc] = [
                h for h in (_host_from_dict(row) for row in rows or []) if h is not None
            ]

    def search(self, attributes: list[Attributes]) -> list[str]:
        """Return hostnames matching any of the given :class:`Attributes`.

        For each query attribute, search the configured location first;
        if it yields no match and the configured location is not
        ``default``, fall back to ``default``.
        """
        results: list[str] = []
        for attribute in attributes:
            host = [
                candidate.name
                for candidate in self.data.get(self.location, [])
                if self.is_candidate_match(candidate, attribute)
            ]

            if not host and self.location != self._default_location:
                host = [
                    candidate.name
                    for candidate in self.data.get(self._default_location, [])
                    if self.is_candidate_match(candidate, attribute)
                ]

            results += host

        return results

    def is_candidate_match(self, candidate: Host, attribute: Attributes) -> bool:
        """Return True iff ``candidate`` satisfies all set fields of ``attribute``."""
        if attribute.arch and attribute.arch != candidate.arch:
            return False

        if attribute.product is not None and not self._product_matches(
            candidate.product, attribute.product
        ):
            return False

        return not (
            attribute.addons
            and not self._addons_match(candidate.addons, attribute.addons)
        )

    @staticmethod
    def _product_matches(candidate: Product, query: Product) -> bool:
        """Return True iff ``candidate`` has the queried name and version."""
        if query.name != candidate.name:
            return False
        if query.version is None:
            return True
        return Refhosts._version_matches(candidate.version, query.version)

    @staticmethod
    def _version_matches(candidate: Version | None, query: Version) -> bool:
        """Match a candidate version against a query version.

        ``query.minor == ""`` is the "candidate must NOT have a minor"
        sentinel; ``query.minor is None`` means "ignore minor".
        """
        if candidate is None:
            return False
        if query.major != candidate.major:
            return False

        if query.minor == "":
            # Sentinel: candidate must have no minor.
            return candidate.minor is None
        if query.minor is None:
            return True
        return query.minor == candidate.minor

    @staticmethod
    def _addons_match(
        candidate_addons: tuple[Addon, ...], query_addons: list[Addon]
    ) -> bool:
        """Return True iff every queried addon is present on the candidate."""
        candidate_by_name = {a.name: a for a in candidate_addons}
        for query in query_addons:
            candidate = candidate_by_name.get(query.name)
            if candidate is None:
                return False
            if query.version is None:
                continue
            if not Refhosts._version_matches(candidate.version, query.version):
                return False
        return True

    def check_location_sanity(self, location: str) -> None:
        """Raise :class:`InvalidLocationError` if ``location`` is unknown."""
        if location not in self.data:
            raise messages.InvalidLocationError(location, self.get_locations())

    def get_locations(self) -> set[str]:
        """Return the set of known location names."""
        return set(self.data.keys())


class RefhostsResolveFailedError(RuntimeError):
    """Raised when no resolver can produce a usable ``refhosts.yml`` source."""


# ---------------------------------------------------------------------------
# Resolvers
# ---------------------------------------------------------------------------


class Resolver(ABC):
    """A strategy for producing a :class:`Refhosts` instance from a source."""

    @abstractmethod
    def resolve(self, config) -> Refhosts:
        """Return a :class:`Refhosts` built from this resolver's source."""


class PathResolver(Resolver):
    """Resolve refhosts from a local file at ``config.refhosts_path``."""

    def __init__(self, refhosts_factory: type[Refhosts] = Refhosts) -> None:
        self.refhosts_factory = refhosts_factory

    def resolve(self, config) -> Refhosts:
        return self.refhosts_factory(config.refhosts_path, config.location)


class HttpsResolver(Resolver):
    """Resolve refhosts from an HTTPS URL, with on-disk caching."""

    def __init__(
        self,
        time_now_getter,
        statter,
        urlopener,
        file_writer,
        cache_path: Path,
        refhosts_factory: type[Refhosts] = Refhosts,
    ) -> None:
        """Initialize the resolver.

        Args:
            time_now_getter: Callable returning the current epoch time.
            statter: Callable returning file stats (``os.stat``).
            urlopener: Callable returning an HTTP response object.
            file_writer: Callable ``(bytes, path) -> None`` to persist
                the downloaded payload.
            cache_path: On-disk cache path for the downloaded YAML.
            refhosts_factory: Factory for the :class:`Refhosts` instance.

        """
        self._time_now = time_now_getter
        self._stat = statter
        self._urlopen = urlopener
        self._write_file = file_writer
        self.cache_path = cache_path
        self.refhosts_factory = refhosts_factory

    def resolve(self, config) -> Refhosts:
        self._refresh_if_needed(config)
        return self.refhosts_factory(self.cache_path, config.location)

    def _refresh_if_needed(self, config) -> None:
        if self._is_refresh_needed(config.refhosts_https_expiration):
            self._refresh(config.refhosts_https_uri)

    def _is_refresh_needed(self, expiration: int) -> bool:
        try:
            statinfo = self._stat(self.cache_path)
        except OSError as e:
            if e.errno == errno.ENOENT:
                return True
            raise

        return self._time_now() - statinfo.st_mtime > expiration

    def _refresh(self, uri: str) -> None:
        self._write_file(self._urlopen(uri).read(), self.cache_path)


# ---------------------------------------------------------------------------
# Factory
# ---------------------------------------------------------------------------


class _RefhostsFactory:
    """Dispatches to a configured :class:`Resolver` to build a :class:`Refhosts`."""

    def __init__(self, resolvers: dict[str, Resolver]) -> None:
        """Initialize the factory.

        Args:
            resolvers: Mapping of resolver name → resolver instance. The
                ``config.refhosts_resolvers`` comma-separated string
                selects which resolvers to try, in order.

        """
        self.resolvers = resolvers

    def __call__(self, config) -> Refhosts:
        """Try each configured resolver in order; return the first success."""
        for name in (x.strip() for x in config.refhosts_resolvers.split(",")):
            resolver = self.resolvers.get(name)
            if resolver is None:
                logger.warning("Refhosts: invalid resolver: %s", name)
                continue
            try:
                return resolver.resolve(config)
            except Exception:
                logger.warning("Refhosts: resolver %s failed", name)
                logger.debug(format_exc())

        raise RefhostsResolveFailedError()


RefhostsFactory = _RefhostsFactory(
    {
        "https": HttpsResolver(
            time.time,
            os.stat,
            urlopen,
            atomic_write_file,
            save_cache_path("refhosts.yml"),
        ),
        "path": PathResolver(),
    }
)
