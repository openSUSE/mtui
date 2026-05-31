"""Typed schema for ``refhosts.yml`` rows and search queries.

Dataclasses (:class:`Version`, :class:`Product`, :class:`Addon`,
:class:`Host`, :class:`Attributes`) plus the free parsing helpers used
by :meth:`Attributes.from_testplatform` and the YAML loader.
"""

import copy
import re
from dataclasses import dataclass, field
from logging import getLogger
from typing import Any, final

logger = getLogger("mtui.refhost")


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
