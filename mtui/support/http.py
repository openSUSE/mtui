"""Single source of truth for outbound HTTP timeout and TLS policy.

Every place in mtui that talks to an HTTP(S) service (the Gitea PR
client, the QEM Dashboard client, the openQA / QAM Dashboard search)
historically defined its own ``(connect, read)`` timeout constant and
made its own, inconsistent decision about TLS certificate
verification. This module centralises both:

- :data:`HTTP_TIMEOUT` is the one ``(connect, read)`` timeout tuple
  shared by all callers. It bounds a stuck socket so a broken network
  cannot hang mtui indefinitely.
- :func:`resolve_verify` turns a per-call-site default plus an optional
  user override (``[mtui] ssl_verify`` in the config) into the
  effective ``verify`` value passed to :mod:`requests`.
- :func:`build_session` / :func:`disable_insecure_warnings` make the
  ``urllib3`` ``InsecureRequestWarning`` suppression happen in exactly
  one place, and only when verification is actually disabled.

Most internal SUSE hosts (openqa.suse.de, dashboard.qam.suse.de,
qam.suse.de, internal mirrors) present self-signed or internal-CA
certificates that the user's system trust store does not know about,
so several call sites disable verification by default. A user who has
the SUSE CA installed can flip verification back on globally by setting
``[mtui] ssl_verify = true`` (or point at a CA bundle with
``ssl_verify = /path/to/ca.pem``).
"""

from __future__ import annotations

import requests
import urllib3
from urllib3.exceptions import InsecureRequestWarning

#: Shared ``(connect, read)`` timeout in seconds for every outbound HTTP
#: call. Bounds a stuck socket so a broken network can't hang mtui.
HTTP_TIMEOUT: tuple[float, float] = (5.0, 30.0)

#: A ``requests``-compatible ``verify`` value: ``True`` (use the system
#: trust store), ``False`` (skip verification), or a path to a CA
#: bundle file.
VerifyPolicy = bool | str

_warnings_disabled = False


def disable_insecure_warnings() -> None:
    """Silence ``urllib3``'s per-request ``InsecureRequestWarning`` once.

    Idempotent: the first call disables the warning process-wide and
    subsequent calls are cheap no-ops. Callers invoke this only when
    they have deliberately disabled certificate verification, so the
    REPL output stays readable instead of emitting a warning per
    request.
    """
    global _warnings_disabled
    if not _warnings_disabled:
        urllib3.disable_warnings(InsecureRequestWarning)
        _warnings_disabled = True


def resolve_verify(
    default: VerifyPolicy, override: VerifyPolicy | None = None
) -> VerifyPolicy:
    """Pick the effective ``verify`` value for a request.

    Args:
        default: The call site's own default (preserves historical
            per-host behaviour when the user has set no global policy).
        override: The user's global ``[mtui] ssl_verify`` setting, or
            ``None`` when unset. When not ``None`` it wins over
            ``default``.

    Returns:
        The ``verify`` value to hand to :mod:`requests`.

    """
    return default if override is None else override


def build_session(verify: VerifyPolicy) -> requests.Session:
    """Create a :class:`requests.Session` with a fixed ``verify`` policy.

    When ``verify`` is falsy, the module-wide insecure-request warning
    is suppressed so callers do not have to repeat that boilerplate.
    """
    session = requests.Session()
    session.verify = verify
    if not verify:
        disable_insecure_warnings()
    return session
