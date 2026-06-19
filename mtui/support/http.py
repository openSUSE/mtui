"""Single source of truth for outbound HTTP timeout and TLS policy.

Every place in mtui that talks to an HTTP(S) service (the Gitea PR
client, the QEM Dashboard client, the openQA / QAM Dashboard search,
the openQA install/result-log downloads, and the ``refhosts.yml``
fetch) historically defined its own ``(connect, read)`` timeout
constant and made its own, inconsistent decision about TLS certificate
verification. This module centralises both:

- :data:`HTTP_TIMEOUT` is the one ``(connect, read)`` timeout tuple
  shared by all callers. It bounds a stuck socket so a broken network
  cannot hang mtui indefinitely.
- :func:`resolve_verify` turns the user's ``[mtui] ssl_verify`` config
  value into the effective ``verify`` passed to :mod:`requests`,
  defaulting to ``True`` (verify) when the value is unset.
- :func:`build_session` / :func:`disable_insecure_warnings` make the
  ``urllib3`` ``InsecureRequestWarning`` suppression happen in exactly
  one place, and only when verification is actually disabled.
- :func:`get_bytes` is the shared GET-to-bytes path used by the
  download sites that previously reached for raw ``urllib`` (which had
  no shared timeout and a hard-coded TLS posture).

Verification is **on by default for every call site**. Several internal
SUSE hosts (openqa.suse.de, dashboard.qam.suse.de, qam.suse.de,
internal mirrors) present internal-CA certificates, so reaching them
out of the box requires the SUSE CA in the system trust store. A user
who cannot install that CA can disable verification globally with
``[mtui] ssl_verify = false`` (or point at a CA bundle with
``ssl_verify = /path/to/ca.pem``).
"""

from __future__ import annotations

import ssl

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
        default: The fallback used when ``override`` is ``None``. Call
            sites pass ``True`` so verification is on whenever the user
            has expressed no preference.
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


def get_bytes(
    url: str,
    *,
    verify: VerifyPolicy,
    timeout: tuple[float, float] = HTTP_TIMEOUT,
) -> bytes:
    """GET ``url`` and return the raw response body as bytes.

    The single GET-to-bytes path for callers that just want a payload
    (a log file, a YAML document) rather than a streaming response.
    Applies the shared :data:`HTTP_TIMEOUT` and the given ``verify``
    policy, and raises for any non-2xx status.

    Args:
        url: The URL to fetch.
        verify: The :data:`VerifyPolicy` to apply (resolve it from the
            call site default and config via :func:`resolve_verify`).
        timeout: The ``(connect, read)`` timeout; defaults to the
            shared :data:`HTTP_TIMEOUT`.

    Returns:
        The response body as ``bytes``.

    Raises:
        requests.exceptions.RequestException: On any transport failure
            or non-2xx HTTP status.

    """
    session = build_session(verify)
    response = session.get(url, timeout=timeout)
    response.raise_for_status()
    return response.content


def is_ssl_verification_error(exc: BaseException) -> bool:
    """Return ``True`` if ``exc`` is (or was caused by) a TLS cert failure.

    ``requests`` wraps the underlying :class:`ssl.SSLCertVerificationError`
    several layers deep (``requests.exceptions.SSLError`` ->
    ``urllib3`` ``MaxRetryError`` -> ``ssl`` error), so this walks the
    ``__cause__``/``__context__`` chain and also matches by message as a
    last resort for transports that stringify the cause.
    """
    seen: set[int] = set()
    current: BaseException | None = exc
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        if isinstance(current, ssl.SSLCertVerificationError):
            return True
        if isinstance(current, requests.exceptions.SSLError):
            return True
        current = current.__cause__ or current.__context__
    return "CERTIFICATE_VERIFY_FAILED" in str(exc)


def ssl_verification_hint(host: str | None = None) -> str:
    """A short, actionable message for a TLS certificate-verification failure.

    Aimed at non-technical users who hit an internal-CA host without the
    SUSE CA installed: it names the two concrete remedies instead of
    dumping a multi-frame traceback.
    """
    where = f" to {host}" if host else ""
    return (
        f"TLS certificate verification failed{where}. The server's "
        "certificate could not be verified against your system's trust "
        "store. To fix this, either install the SUSE root CA in your "
        "system trust store, or disable verification by setting "
        "'ssl_verify = false' under the [mtui] section of your mtui config "
        "(e.g. ~/.mtuirc). You can also point 'ssl_verify' at a CA bundle "
        "file: 'ssl_verify = /path/to/ca.pem'."
    )
