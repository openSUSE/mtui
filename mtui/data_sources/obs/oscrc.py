"""Native reader for OBS/IBS credentials from oscrc (no osc import).

Parses the user's existing oscrc with the stdlib :mod:`configparser` and
resolves the credentials for one apiurl (the fixed ``https://api.suse.de``)
into a small frozen dataclass. The oscrc is located exactly like ``osc``
itself (``$OSC_CONFIG`` → ``$XDG_CONFIG_HOME/osc/oscrc`` → ``~/.oscrc``),
so mtui reads the same file ``osc`` does without importing it. mtui
authenticates itself with SSH-signature auth, so this reader deliberately
does **not** read ``pass``/``passx`` for that Signature-only target —
pulling a plaintext password into memory for a code path that never fires
would be pure exposure. Every failure is a typed, fail-closed
:class:`ObsConfigError` that names the real oscrc file/section; there is no
interactive prompt.
"""

from __future__ import annotations

import configparser
import os
import re
import stat
from dataclasses import dataclass
from logging import getLogger
from pathlib import Path

from xdg.BaseDirectory import xdg_config_home

from ...support.exceptions import ObsConfigError

logger = getLogger("mtui.data_sources.obs.oscrc")

# An ``sshkey`` value like ``SHA256:abc...`` names a key held by the ssh
# agent by fingerprint rather than a file on disk; the native backend
# resolves it through the agent at signing time.
_FINGERPRINT_RE = re.compile(r"^[A-Z0-9]+:")

# credentials_mgr_class values whose secret lives outside the oscrc (a system
# keyring, or a transient prompt), so the native backend can never read it and
# will not prompt. Only consulted when no usable 'sshkey' is configured: a
# working key authenticates by signature regardless of the manager.
_UNSUPPORTED_MGR = ("keyring", "transient")


@dataclass(frozen=True, slots=True)
class ObsCredentials:
    """Resolved OBS Signature-auth credentials for one apiurl.

    Exactly one of ``sshkey_path`` (a private-key file on disk) or
    ``sshkey_fingerprint`` (an ssh-agent key's ``SHA256:…`` fingerprint)
    identifies the signing key. Carries no password by construction: the
    native backend uses SSH signature auth against api.suse.de, so
    ``pass``/``passx`` are never read for that target.
    """

    apiurl: str
    user: str
    source: str
    sshkey_path: Path | None = None
    sshkey_fingerprint: str | None = None


def _default_conffile() -> Path:
    """Locate the oscrc exactly like ``osc`` (its ``identify_conf``).

    Precedence, mirroring upstream ``osc``:

    1. ``$OSC_CONFIG`` (verbatim, if set) — the explicit override.
    2. ``$XDG_CONFIG_HOME/osc/oscrc`` (default ``~/.config/osc/oscrc``) if
       it exists.
    3. ``~/.oscrc`` if it exists.
    4. otherwise the XDG path, as the fallback default.

    If both the XDG file and ``~/.oscrc`` exist, the XDG one wins and a
    warning is logged (a dangling ``~/.oscrc`` symlink counts as present).
    """
    override = os.environ.get("OSC_CONFIG")
    if override is not None:
        return Path(override).expanduser()

    xdg_path = Path(xdg_config_home) / "osc" / "oscrc"
    home_path = Path("~/.oscrc").expanduser()

    if xdg_path.exists():
        if home_path.exists() or home_path.is_symlink():
            logger.warning(
                "multiple oscrc files detected; ignoring %s, using %s",
                home_path,
                xdg_path,
            )
        return xdg_path
    if home_path.exists():
        return home_path
    return xdg_path


def _resolve_sshkey(raw: str) -> tuple[Path | None, str | None]:
    """Resolve an oscrc ``sshkey`` value to ``(path, fingerprint)``.

    A ``SHA256:…`` (or other ``ALG:…``) value names an ssh-agent key by
    fingerprint and yields ``(None, fingerprint)``. Otherwise it is a
    private-key file: a bare name (``id_ed25519``) resolves under
    ``~/.ssh/``; a value containing ``/`` is a literal (``~``-expanded) path,
    yielding ``(path, None)``.

    Raises:
        ObsConfigError: If the value is empty.

    """
    value = raw.strip()
    if not value:
        raise ObsConfigError("oscrc 'sshkey' is empty")
    if _FINGERPRINT_RE.match(value):
        return None, value
    if "/" in value:
        return Path(value).expanduser(), None
    return Path("~/.ssh").expanduser() / value, None


def read_credentials(apiurl: str) -> ObsCredentials:
    """Read SSH-signature credentials for ``apiurl`` from oscrc.

    The oscrc is located like ``osc`` (see :func:`_default_conffile`):
    ``$OSC_CONFIG`` → ``$XDG_CONFIG_HOME/osc/oscrc`` → ``~/.oscrc``.

    Args:
        apiurl: The OBS API URL whose oscrc section to read (its section
            header must equal this value).

    Returns:
        The resolved :class:`ObsCredentials` (user + private-key path).

    Raises:
        ObsConfigError: For any fault — missing/unreadable oscrc, missing
            section/user/sshkey, an unsupported credentials manager, an
            agent-fingerprint sshkey, or a missing key file. The message
            names the real failing file/section. Never prompts.

    """
    path = _default_conffile()
    if not path.is_file():
        raise ObsConfigError(
            f"osc config file not found: {path}; create an oscrc with a "
            f"[{apiurl}] section (e.g. run 'osc -A {apiurl} whoami' once)"
        )

    try:
        mode = path.stat().st_mode
        if mode & (stat.S_IRWXG | stat.S_IRWXO):
            logger.warning(
                "oscrc %s is group/world-accessible; tighten it to 0600", path
            )
    except OSError:  # pragma: no cover - stat rarely fails after is_file()
        pass

    parser = configparser.ConfigParser(interpolation=None)
    try:
        with path.open(encoding="utf-8") as handle:
            parser.read_file(handle)
    except (OSError, configparser.Error) as e:
        # Never interpolate the raw exception: configparser errors embed the
        # offending source line, which could be a ``pass = <secret>`` entry.
        raise ObsConfigError(f"could not parse oscrc {path}: {type(e).__name__}") from e

    # osc normalises trailing path slashes when matching apiurl sections
    # (sanitize_apiurl), so [https://api.suse.de/] matches api.suse.de too.
    wanted = apiurl.rstrip("/")
    section_name = next(
        (name for name in parser.sections() if name.rstrip("/") == wanted), None
    )
    if section_name is None:
        raise ObsConfigError(
            f"oscrc {path} has no [{apiurl}] section; the native OBS backend "
            "reads credentials from the section whose header equals the apiurl"
        )
    section = parser[section_name]

    # ``sshkey`` (and any credentials manager) inherit from [general] when the
    # host section omits them, matching osc's FromParent resolution; ``user``
    # does not inherit (osc requires it per host).
    def _inherited(key: str) -> str:
        value = section.get(key, "").strip()
        if not value and parser.has_section("general"):
            value = parser["general"].get(key, "").strip()
        return value

    user = section.get("user", "").strip()
    if not user:
        raise ObsConfigError(f"oscrc [{apiurl}] has no 'user'")

    # Resolve the ssh key FIRST, and ignore ``credentials_mgr_class`` entirely
    # whenever a usable key exists. osc itself orders Signature auth ahead of
    # Basic and explicitly disables the password path for the transient
    # manager, so an oscrc carrying both an 'sshkey' and a password manager
    # authenticates by signature under osc too. Rejecting such an oscrc up
    # front turned a perfectly working configuration into a hard failure.
    sshkey = _inherited("sshkey")
    if not sshkey:
        # Only now does the credentials manager matter. Unlike 'sshkey', osc
        # does not inherit ``credentials_mgr_class`` from [general], so read it
        # from the host section only.
        mgr = section.get("credentials_mgr_class", "").strip()
        if mgr and any(bad in mgr.lower() for bad in _UNSUPPORTED_MGR):
            raise ObsConfigError(
                f"oscrc [{apiurl}] has no 'sshkey' and uses "
                f"credentials_mgr_class={mgr!r}, whose secret is not stored in "
                "the file; the native OBS backend cannot read it and never "
                "prompts. Add an 'sshkey' entry to authenticate by SSH signature."
            )
        raise ObsConfigError(
            f"oscrc [{apiurl}] has no 'sshkey' (in the section or [general]); the "
            "native OBS backend requires SSH-signature auth (plaintext-password "
            "auth is not supported)"
        )
    # NB: 'pass'/'passx' are intentionally never read for this Signature-only
    # target — see the module docstring.

    key_path, fingerprint = _resolve_sshkey(sshkey)
    if key_path is not None and not _key_available(key_path):
        raise ObsConfigError(
            f"ssh key {key_path} (from oscrc sshkey={sshkey!r}) does not exist"
        )

    return ObsCredentials(
        apiurl=apiurl,
        user=user,
        source=str(path),
        sshkey_path=key_path,
        sshkey_fingerprint=fingerprint,
    )


def _key_available(key_path: Path) -> bool:
    """A key is usable if its private file or its ``.pub`` (agent) exists.

    A ``.pub``-only key on disk is signed via an ssh-agent that holds the
    private half (matched by public blob at auth time).
    """
    return key_path.is_file() or Path(f"{key_path}.pub").is_file()
