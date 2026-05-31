"""``refhosts.yml`` loader, search engine, and resolver-dispatch factory.

The :class:`Refhosts` loader parses YAML rows into :class:`~mtui.hosts.refhost.models.Host`
objects and answers :class:`~mtui.hosts.refhost.models.Attributes` queries.
The :class:`_RefhostsFactory` walks a configured resolver chain to obtain
the YAML source; the bound :data:`RefhostsFactory` singleton lives in
:mod:`mtui.hosts.refhost` (the package ``__init__``) so it can wire in
the concrete resolvers without a circular import.
"""

from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import TYPE_CHECKING

from ruamel.yaml import YAML

from ...support import messages
from .models import Addon, Attributes, Host, Product, Version, _host_from_dict

if TYPE_CHECKING:
    from .resolvers import Resolver

logger = getLogger("mtui.refhost")


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


class _RefhostsFactory:
    """Dispatches to a configured :class:`Resolver` to build a :class:`Refhosts`."""

    def __init__(self, resolvers: "dict[str, Resolver]") -> None:
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
