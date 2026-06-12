"""Reference-host schema, YAML loader, and resolver registry.

Re-exports the public surface and the private :class:`_RefhostsFactory`
class to preserve the whitebox-test access patterns established before
the split (see ``tests/test_refhost.py``, which patches
``refhost._RefhostsFactory`` and ``refhost.HttpsResolver`` by name).
"""

import os
import time

from ...support.fileops import atomic_write_file
from ...support.http import VerifyPolicy, get_bytes
from ...support.paths import save_cache_path
from .models import Addon, Attributes, Host, Product, Version
from .resolvers import HttpsResolver, PathResolver, Resolver
from .store import Refhosts, RefhostsResolveFailedError, _RefhostsFactory


class _BytesResponse:
    """Minimal ``.read()``-able wrapper so the resolver stays transport-agnostic."""

    def __init__(self, data: bytes) -> None:
        self._data = data

    def read(self) -> bytes:
        return self._data


def _requests_urlopener(uri: str, verify: VerifyPolicy) -> _BytesResponse:
    """Fetch ``uri`` via the shared HTTP helper, honoring the verify policy."""
    return _BytesResponse(get_bytes(uri, verify=verify))


RefhostsFactory = _RefhostsFactory(
    {
        "https": HttpsResolver(
            time.time,
            os.stat,
            _requests_urlopener,
            atomic_write_file,
            save_cache_path("refhosts.yml"),
        ),
        "path": PathResolver(),
    }
)


__all__ = [
    "Addon",
    "Attributes",
    "Host",
    "HttpsResolver",
    "PathResolver",
    "Product",
    "Refhosts",
    "RefhostsFactory",
    "RefhostsResolveFailedError",
    "Resolver",
    "Version",
    "_RefhostsFactory",
]
