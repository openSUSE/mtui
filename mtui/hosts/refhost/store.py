"""``refhosts.yml`` loader, search engine, and resolver-dispatch factory.

The :class:`Refhosts` loader parses YAML rows into :class:`~mtui.hosts.refhost.models.Host`
objects and answers :class:`~mtui.hosts.refhost.models.Attributes` queries.
The :class:`_RefhostsFactory` walks a configured resolver chain to obtain
the YAML source; the bound :data:`RefhostsFactory` singleton lives in
:mod:`mtui.hosts.refhost` (the package ``__init__``) so it can wire in
the concrete resolvers without a circular import.
"""

import fnmatch
from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import TYPE_CHECKING

from ruamel.yaml import YAML

from .models import Addon, Attributes, Host, Product, Version, _host_from_dict

if TYPE_CHECKING:
    from .resolvers import Resolver

logger = getLogger("mtui.refhost")


class Refhosts:
    """Loads and searches ``refhosts.yml`` for hosts matching :class:`Attributes`."""

    def __init__(self, hostmap: Path) -> None:
        """Initialize the Refhosts object.

        Args:
            hostmap: Path to ``refhosts.yml``.

        """
        self._parse_refhosts(hostmap)

    def _parse_refhosts(self, hostmap: Path) -> None:
        """Load ``hostmap`` and convert each row to a :class:`Host`.

        The legacy ``refhosts.yml`` groups host rows under top-level
        location keys (``default:``, ``nuremberg:``, …). Location support
        has been retired, so every group is merged into a single flat list
        of hosts. Malformed rows are logged at ERROR and dropped; YAML
        parse failures propagate.
        """
        try:
            with hostmap.open() as f:
                raw = YAML(typ="safe").load(f)
        except Exception:
            logger.error("failed to parse refhosts.yml")
            raise

        self.data: list[Host] = []
        for rows in (raw or {}).values():
            self.data.extend(
                h for h in (_host_from_dict(row) for row in rows or []) if h is not None
            )

    def search(self, attributes: list[Attributes]) -> list[str]:
        """Return hostnames matching any of the given :class:`Attributes`."""
        results: list[str] = []
        for attribute in attributes:
            results += [
                candidate.name
                for candidate in self.data
                if self.is_candidate_match(candidate, attribute)
            ]
        return results

    def is_candidate_match(self, candidate: Host, attribute: Attributes) -> bool:
        """Return True iff ``candidate`` satisfies all set fields of ``attribute``."""
        if attribute.arch and attribute.arch != candidate.arch:
            return False

        if attribute.product is not None and not self._product_satisfied(
            candidate, attribute.product
        ):
            return False

        return not (
            attribute.addons
            and not self._addons_match(candidate.addons, attribute.addons)
        )

    @staticmethod
    def _product_satisfied(candidate: Host, query: Product) -> bool:
        """Return True iff ``candidate`` provides the queried base product.

        A testplatform ``base=<name>`` is satisfied when the host's base
        product matches, **or** when the host carries that product as an
        addon. Extension products (``SLES-LTSS``, ``sle-ha``, ``SLES_SAP``,
        ``SLE_RT``, …) ship on a ``SLES``/``SLED`` base and are recorded as
        addons in the refhosts-ng schema — one physical host can only have a
        single base, so e.g. ``base=SLES-LTSS`` must still resolve to a
        ``SLES`` host that has the ``SLES-LTSS`` extension installed.
        """
        if Refhosts._product_matches(candidate.product, query):
            return True
        return any(
            addon.name == query.name
            and (
                query.version is None
                or Refhosts._version_matches(addon.version, query.version)
            )
            for addon in candidate.addons
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

    def host_by_name(self, name: str) -> Host | None:
        """Return the refhosts entry whose ``name`` matches, or ``None``.

        So a connected host maps back to the metadata row mtui would use
        for it. Returns ``None`` if no row matches.
        """
        for candidate in self.data:
            if candidate.name == name:
                return candidate
        return None

    def query(
        self,
        *,
        attributes: "list[Attributes] | None" = None,
        name: str | None = None,
        arch: "list[str] | None" = None,
        product: str | None = None,
        version: str | None = None,
        addon: "list[str] | None" = None,
    ) -> list[Host]:
        """Return refhosts matching the filters, de-duplicated by host name.

        ``attributes`` (parsed from a ``testplatform``) and the field filters
        (``name`` glob, ``arch``, ``product`` substring, ``version``,
        ``addon`` substring) are alternatives — when ``attributes`` is given
        the field filters are ignored. With neither, every host is returned.
        """
        seen: set[str] = set()
        out: list[Host] = []
        for host in self.data:
            if host.name in seen:
                continue
            if attributes is not None:
                if not any(self.is_candidate_match(host, a) for a in attributes):
                    continue
            elif not self._field_match(host, name, arch, product, version, addon):
                continue
            seen.add(host.name)
            out.append(host)
        return out

    @staticmethod
    def _field_match(
        host: Host,
        name: str | None,
        arch: "list[str] | None",
        product: str | None,
        version: str | None,
        addon: "list[str] | None",
    ) -> bool:
        """True iff ``host`` satisfies every supplied ad-hoc field filter."""
        if name and not fnmatch.fnmatch(host.name, name):
            return False
        if arch and host.arch not in arch:
            return False
        if product and product.lower() not in host.product.name.lower():
            return False
        if version and not Refhosts._version_str_match(host.product.version, version):
            return False
        if addon:
            have = [a.name.lower() for a in host.addons]
            if not all(any(want.lower() in n for n in have) for want in addon):
                return False
        return True

    @staticmethod
    def _version_str_match(hostver: "Version | None", want: str) -> bool:
        """Loosely match a host version against ``15-SP6`` / ``15.6`` / ``15``.

        ``SP`` is optional and case-insensitive; a bare major matches any
        minor. A host with no version never matches a versioned query.
        """
        if hostver is None:
            return False
        parts = want.replace(".", "-").lower().split("-", 1)
        if str(hostver.major).lower() != parts[0]:
            return False
        if len(parts) == 2 and parts[1]:
            host_minor = "" if hostver.minor is None else str(hostver.minor).lower()
            return host_minor.replace("sp", "") == parts[1].replace("sp", "")
        return True

    @staticmethod
    def slot_of(host: Host) -> tuple[str, str, str, tuple[str, ...]]:
        """Return the test-target slot key for ``host``.

        The slot is the full ``(product, version, arch, addons)`` an update
        distinguishes — not just arch — so an update spanning, say, all arches
        of SLE15-SP5 *and* SP7 still gets one host per (service-pack, arch);
        only genuine duplicates collapse to a single slot (RFC §5.7).
        """
        ver = host.product.version
        if ver is None:
            ver_str = ""
        elif ver.minor is None or ver.minor == "":
            ver_str = str(ver.major)
        else:
            ver_str = f"{ver.major}-{ver.minor}"
        addons = tuple(sorted(a.name for a in host.addons))
        return (host.product.name, ver_str, host.arch, addons)

    def search_pool(
        self,
        attributes: list[Attributes],
    ) -> list[tuple[Host, tuple[str, str, str, tuple[str, ...]]]]:
        """Return pool candidates ``(host, slot)`` matching ``attributes``.

        Thin wrapper over :meth:`query` that tags each match with its
        :meth:`slot_of` test-target slot, so the host-arbitration path can
        group candidates by slot and draw one free host per slot.
        """
        return [
            (host, self.slot_of(host)) for host in self.query(attributes=attributes)
        ]


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
            except Exception as e:
                logger.warning("Refhosts: resolver %s failed: %s", name, e)
                logger.debug(format_exc())

        raise RefhostsResolveFailedError()
