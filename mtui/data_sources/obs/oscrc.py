"""Native reader for OBS/IBS credentials from ``~/.oscrc`` (no osc import).

Parses the user's existing oscrc with the stdlib :mod:`configparser` and
resolves the credentials for one apiurl (the fixed ``https://api.suse.de``)
into a small frozen dataclass. mtui authenticates itself with SSH-signature
auth, so this reader deliberately does **not** read ``pass``/``passx`` for
that Signature-only target — pulling a plaintext password into memory for a
code path that never fires would be pure exposure. Every failure is a typed,
fail-closed :class:`ObsConfigError` that names the real oscrc file/section;
there is no interactive prompt.
"""

from __future__ import annotations

import configparser
import re
import stat
from dataclasses import dataclass
from logging import getLogger
from pathlib import Path

from ...support.exceptions import ObsConfigError

logger = getLogger("mtui.data_sources.obs.oscrc")

# An ``sshkey`` value like ``SHA256:abc...`` names a key held by the ssh
# agent by fingerprint rather than a file on disk. Agent auth is not yet
# supported by the native backend (deferred), so such values fail closed.
_FINGERPRINT_RE = re.compile(r"^[A-Z0-9]+:")

# credentials_mgr_class values that route credentials through a mechanism
# the native SSH-signature backend cannot use.
_UNSUPPORTED_MGR = ("keyring", "transient")


@dataclass(frozen=True, slots=True)
class ObsCredentials:
    """Resolved OBS Signature-auth credentials for one apiurl.

    Carries no password by construction: the native backend uses SSH
    signature auth against api.suse.de, so ``pass``/``passx`` are never
    read for that target.
    """

    apiurl: str
    user: str
    sshkey_path: Path
    source: str


def _default_conffile() -> Path:
    """The default oscrc location (``~/.oscrc``)."""
    return Path("~/.oscrc").expanduser()


def _resolve_sshkey(raw: str) -> Path:
    """Resolve an oscrc ``sshkey`` value to a private-key file path.

    A bare name (``id_ed25519``) resolves under ``~/.ssh/``; a value
    containing ``/`` is treated as a literal (``~``-expanded) path.

    Raises:
        ObsConfigError: If the value is empty, or is an agent fingerprint
            (``SHA256:…``) — ssh-agent auth is deferred and fails closed.

    """
    value = raw.strip()
    if not value:
        raise ObsConfigError("oscrc 'sshkey' is empty")
    if _FINGERPRINT_RE.match(value):
        raise ObsConfigError(
            f"oscrc sshkey {value!r} is an ssh-agent fingerprint; the native "
            "OBS backend does not yet support ssh-agent auth — set 'sshkey' to "
            "a private-key file name or path instead"
        )
    if "/" in value:
        return Path(value).expanduser()
    return Path("~/.ssh").expanduser() / value


def read_credentials(apiurl: str, conffile: str = "") -> ObsCredentials:
    """Read SSH-signature credentials for ``apiurl`` from oscrc.

    Args:
        apiurl: The OBS API URL whose oscrc section to read (its section
            header must equal this value).
        conffile: Optional oscrc path override; empty uses ``~/.oscrc``.

    Returns:
        The resolved :class:`ObsCredentials` (user + private-key path).

    Raises:
        ObsConfigError: For any fault — missing/unreadable oscrc, missing
            section/user/sshkey, an unsupported credentials manager, an
            agent-fingerprint sshkey, or a missing key file. The message
            names the real failing file/section. Never prompts.

    """
    path = Path(conffile).expanduser() if conffile else _default_conffile()
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

    mgr = _inherited("credentials_mgr_class")
    if mgr and any(bad in mgr.lower() for bad in _UNSUPPORTED_MGR):
        raise ObsConfigError(
            f"oscrc [{apiurl}] uses credentials_mgr_class={mgr!r}; the native "
            "OBS backend supports only SSH-signature auth (an 'sshkey' entry) — "
            "keyring/transient-password managers are not supported"
        )

    user = section.get("user", "").strip()
    if not user:
        raise ObsConfigError(f"oscrc [{apiurl}] has no 'user'")

    sshkey = _inherited("sshkey")
    if not sshkey:
        raise ObsConfigError(
            f"oscrc [{apiurl}] has no 'sshkey' (in the section or [general]); the "
            "native OBS backend requires SSH-signature auth (plaintext-password "
            "auth is not supported)"
        )
    # NB: 'pass'/'passx' are intentionally never read for this Signature-only
    # target — see the module docstring.

    key_path = _resolve_sshkey(sshkey)
    if not key_path.is_file():
        raise ObsConfigError(
            f"ssh key {key_path} (from oscrc sshkey={sshkey!r}) does not exist"
        )

    return ObsCredentials(
        apiurl=apiurl, user=user, sshkey_path=key_path, source=str(path)
    )
