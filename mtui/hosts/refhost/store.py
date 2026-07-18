"""``refhosts.yml`` loader, search engine, and resolver-dispatch factory.

The :class:`Refhosts` loader parses YAML rows into :class:`~mtui.hosts.refhost.models.Host`
objects and answers :class:`~mtui.hosts.refhost.models.Attributes` queries.
The :class:`_RefhostsFactory` walks a configured resolver chain to obtain
the YAML source; the bound :data:`RefhostsFactory` singleton lives in
:mod:`mtui.hosts.refhost` (the package ``__init__``) so it can wire in
the concrete resolvers without a circular import.
"""

import fnmatch
import os
import threading
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
            # Explicit UTF-8: the HTTPS-downloaded cache is written UTF-8
            # (atomic_write_file), so reading with the locale codec would
            # mis-decode non-ASCII content under a non-UTF-8 locale.
            with hostmap.open(encoding="utf-8") as f:
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

    @staticmethod
    def slot_for_query(
        attribute: Attributes, host: Host
    ) -> tuple[str, str, str, tuple[str, ...]]:
        """Return the test-target slot keyed on the *queried* attributes.

        Unlike :meth:`slot_of` — which keys on every module a host happens to
        have installed — this keys on what the testplatform actually
        distinguishes: the base product + version it requests, the host's arch
        (testplatforms fan out one query per arch), and only the addons the
        testplatform explicitly asked for. Hosts that satisfy the same query
        are interchangeable for that update and must collapse to one slot, so
        the arbiter draws a single host per (product, version, arch, requested
        addons) instead of one per distinct installed-module set.
        """
        product = attribute.product
        if product is None:
            name, ver_str = "", ""
        else:
            name = product.name
            ver = product.version
            if ver is None:
                ver_str = ""
            elif ver.minor is None or ver.minor == "":
                ver_str = str(ver.major)
            else:
                ver_str = f"{ver.major}-{ver.minor}"
        addons = tuple(sorted(a.name for a in attribute.addons))
        return (name, ver_str, host.arch, addons)

    def search_pool_by_query(
        self,
        attributes: list[Attributes],
    ) -> list[tuple[Host, tuple[str, str, str, tuple[str, ...]]]]:
        """Return pool candidates ``(host, slot)`` keyed on the query slot.

        Like :meth:`search_pool` but tags each match with
        :meth:`slot_for_query` (the testplatform's requested identity) rather
        than the host's full installed-module identity, so host-arbitration
        draws one host per *requested* test-target slot. Each host is tagged
        with the slot of the first attribute it matches.
        """
        out: list[tuple[Host, tuple[str, str, str, tuple[str, ...]]]] = []
        seen: set[str] = set()
        for host in self.query(attributes=attributes):
            if host.name in seen:
                continue
            for attribute in attributes:
                if self.is_candidate_match(host, attribute):
                    out.append((host, self.slot_for_query(attribute, host)))
                    seen.add(host.name)
                    break
        return out


class RefhostsResolveFailedError(RuntimeError):
    """Raised when no resolver can produce a usable ``refhosts.yml`` source."""


# Process-wide single-flight cache of parsed refhosts stores. ``refhosts.yml``
# is ~68KB and its ruamel parse is ~1s of GIL-held CPU; under the ``mtui-mcp``
# http transport hundreds of concurrent sessions would otherwise each re-parse
# it, serialising the interpreter on that one core. A parsed :class:`Refhosts`
# is read-only, so one instance is safely shared across every caller.
_refhosts_cache: "dict[tuple[str, int, int], Refhosts]" = {}
_refhosts_cache_lock = threading.Lock()
#: A handful of distinct sources at most (the local path + the HTTPS cache,
#: plus room for tests); the bound just keeps a leak impossible.
_REFHOSTS_CACHE_MAXSIZE = 8


def load_refhosts(hostmap: Path) -> Refhosts:
    """Return a process-wide cached :class:`Refhosts` for ``hostmap``.

    Collapses the ~1s ruamel parse to once per file *version* across the whole
    process, shared by every resolver and session. The cache key is
    ``(resolved path, st_mtime_ns, st_size)`` so an edited file -- or the
    periodic HTTPS cache refresh -- is picked up on its next load. (This
    relies on the filesystem's ``st_mtime_ns`` resolution: a byte-identical-
    size in-place edit landing in the same mtime bucket on a coarse-mtime
    mount would be missed until the mtime advances or the process restarts.
    The default ``https``-first resolver is immune -- it rewrites the cache
    via atomic rename -- and the ``path`` fallback is an RPM-managed file on
    a nanosecond-mtime root filesystem.) A single
    lock held across the parse (not a bare ``functools.lru_cache``) makes
    concurrent cold-cache callers wait for one parse instead of stampeding
    into N simultaneous 1s parses. Cache *hits* are lock-free.

    A source that cannot be ``stat``-ed is not cached: :class:`Refhosts` is
    constructed directly so the real error surfaces exactly as before.
    """
    try:
        st = hostmap.stat()
        key = (os.fspath(hostmap.resolve()), st.st_mtime_ns, st.st_size)
    except OSError:
        return Refhosts(hostmap)

    cached = _refhosts_cache.get(key)
    if cached is not None:
        return cached
    with _refhosts_cache_lock:
        cached = _refhosts_cache.get(key)
        if cached is None:
            cached = Refhosts(hostmap)
            if len(_refhosts_cache) >= _REFHOSTS_CACHE_MAXSIZE:
                # FIFO-evict the oldest entry (dict preserves insertion order).
                del _refhosts_cache[next(iter(_refhosts_cache))]
            _refhosts_cache[key] = cached
    return cached


def _clear_refhosts_cache() -> None:
    """Drop the process-wide refhosts parse cache (test hook)."""
    with _refhosts_cache_lock:
        _refhosts_cache.clear()


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
