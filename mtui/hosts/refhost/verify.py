"""Compare a connected host's installed products against ``refhosts.yml``.

When mtui connects a reference host it can know two things about that host's
products: what is *actually installed* (parsed from ``/etc/products.d`` into a
:class:`~mtui.types.systems.System`) and what the ``refhosts.yml`` metadata
*says* should be there (a :class:`~mtui.hosts.refhost.models.Host` row).

:func:`compare` checks the two for drift -- a wrong or wrong-version base
product, a wrong architecture, missing or extra addons, or a dangling
``baseproduct`` symlink -- so that validating an update on a host that is not
the system we think it is gets surfaced. The check is advisory: callers warn
and keep the host.

Normalization (grounded on real refhosts across SLE 15/16 and SL-Micro):

* Detected product *names* use the same identifiers as ``refhosts.yml``
  (``SLES``, ``SLE_RT``, ``SLES-LTSS``, ``SL-Micro``, ``sle-module-*`` ...);
  compared case-insensitively for safety.
* Detected *version strings* come from
  :func:`mtui.hosts.target.parsers.product.parse_product`:
  ``"15-SP4"`` (service packs), ``"16.0"`` / ``"6.1"`` (dotted). Both are
  normalized to a refhosts :class:`~mtui.hosts.refhost.models.Version`.
* ``qa`` is dropped from the comparison on the refhosts side because
  :func:`mtui.hosts.target.parsers.system.parse_system` intentionally skips
  ``qa.prod`` on the detected side -- keeping it would report a phantom
  "missing qa" on every host whose metadata lists it.
"""

import re
from dataclasses import dataclass, field

from .models import Host, Version

#: Addon names parse_system never reports as installed (it skips
#: ``qa.prod``); excluded from the refhosts side too so they are not
#: reported as missing. Compared case-folded.
_IGNORED_ADDONS = frozenset({"qa"})


def _int_or_str(value: str) -> int | str:
    """Return ``int(value)`` when numeric, else the original string."""
    try:
        return int(value)
    except ValueError:
        return value


def normalize_version(version: str) -> Version | None:
    """Convert a detected version string to a refhosts :class:`Version`.

    Handles the formats emitted by ``parse_product``:

    * ``"15-SP4"`` -> ``Version(15, "SP4")`` (SLE 12/15 service packs)
    * ``"16.0"`` / ``"6.1"`` -> ``Version(16, 0)`` / ``Version(6, 1)``
    * ``"15"`` -> ``Version(15, None)``
    * ``""`` -> ``None``
    """
    version = version.strip()
    if not version:
        return None
    if m := re.fullmatch(r"(\d+)-SP(\d+)", version):
        return Version(major=int(m.group(1)), minor=f"SP{m.group(2)}")
    if "." in version:
        major, _, minor = version.partition(".")
        return Version(major=_int_or_str(major), minor=_int_or_str(minor))
    return Version(major=_int_or_str(version), minor=None)


def _fmt_version(version: Version | None) -> str:
    """Render a :class:`Version` for warning messages (``""`` when None)."""
    if version is None:
        return ""
    if version.minor is None or version.minor == "":
        return str(version.major)
    sep = "." if isinstance(version.minor, int) else "-"
    return f"{version.major}{sep}{version.minor}"


@dataclass
class ProductDiff:
    """Outcome of comparing detected products against refhosts metadata."""

    #: ``/etc/products.d/baseproduct`` is a dangling symlink.
    dangling_base: bool = False
    #: Human-readable base product mismatch, or ``None`` when the base matches.
    base_mismatch: str | None = None
    #: Addons in ``refhosts.yml`` but not installed (``"name x.y"``).
    missing_addons: list[str] = field(default_factory=list)
    #: Addons installed but not in ``refhosts.yml`` (``"name x.y"``).
    extra_addons: list[str] = field(default_factory=list)
    #: Addons present on both sides with a differing version.
    mismatched_addons: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        """True when no drift was detected."""
        return not (
            self.dangling_base
            or self.base_mismatch
            or self.missing_addons
            or self.extra_addons
            or self.mismatched_addons
        )

    def warnings(self) -> list[str]:
        """Return one warning line per drift class (empty when :attr:`ok`)."""
        out: list[str] = []
        if self.dangling_base:
            out.append("dangling /etc/products.d/baseproduct symlink")
        if self.base_mismatch:
            out.append(f"base product mismatch: {self.base_mismatch}")
        if self.missing_addons:
            out.append(
                "addons in metadata but not installed: "
                + ", ".join(sorted(self.missing_addons))
            )
        if self.extra_addons:
            out.append(
                "addons installed but not in metadata: "
                + ", ".join(sorted(self.extra_addons))
            )
        if self.mismatched_addons:
            out.append(
                "addons with version mismatch: "
                + ", ".join(sorted(self.mismatched_addons))
            )
        return out


def _named(name: str, version: Version | None) -> str:
    """Render ``"name x.y"`` (or just ``"name"`` when version is None)."""
    rendered = _fmt_version(version)
    return f"{name} {rendered}" if rendered else name


def compare(system, host: Host) -> ProductDiff:
    """Compare a detected :class:`System` against a refhosts :class:`Host`.

    Args:
        system: The :class:`~mtui.types.systems.System` parsed from the host.
        host: The :class:`Host` row from ``refhosts.yml`` for the same host.

    Returns:
        A :class:`ProductDiff` describing any base/addon/symlink drift.

    """
    diff = ProductDiff(dangling_base=bool(getattr(system, "dangling_base", False)))

    base = system.get_base()
    expected = host.product

    # Base product: skip the name/version check when the base is a dangling
    # placeholder (the dangling warning already covers it), but still check
    # the architecture if we have one.
    problems: list[str] = []
    if not diff.dangling_base:
        if base.name.casefold() != expected.name.casefold():
            problems.append(f"name {base.name!r} != {expected.name!r} (metadata)")
        detected_version = normalize_version(base.version)
        if expected.version is not None and detected_version != expected.version:
            problems.append(
                f"version {base.version!r} != "
                f"{_fmt_version(expected.version)!r} (metadata)"
            )
    if base.arch and host.arch and base.arch != host.arch:
        problems.append(f"arch {base.arch!r} != {host.arch!r} (metadata)")
    if problems:
        diff.base_mismatch = "; ".join(problems)

    # Addons: case-folded name -> (display name, normalized version), with the
    # always-ignored addons (qa) dropped from both sides.
    detected = {
        p.name.casefold(): (p.name, normalize_version(p.version))
        for p in system.get_addons()
        if p.name.casefold() not in _IGNORED_ADDONS
    }
    metadata = {
        a.name.casefold(): (a.name, a.version)
        for a in host.addons
        if a.name.casefold() not in _IGNORED_ADDONS
    }

    for key in metadata.keys() - detected.keys():
        name, version = metadata[key]
        diff.missing_addons.append(_named(name, version))
    for key in detected.keys() - metadata.keys():
        name, version = detected[key]
        diff.extra_addons.append(_named(name, version))
    for key in detected.keys() & metadata.keys():
        det_name, det_version = detected[key]
        meta_name, meta_version = metadata[key]
        if meta_version is not None and det_version != meta_version:
            diff.mismatched_addons.append(
                f"{det_name} (installed {_fmt_version(det_version)} != "
                f"metadata {_fmt_version(meta_version)})"
            )

    return diff
