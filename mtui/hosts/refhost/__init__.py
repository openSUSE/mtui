"""Reference-host schema, YAML loader, and resolver registry.

Re-exports the public surface and the private :class:`_RefhostsFactory`
class to preserve the whitebox-test access patterns established before
the split (see ``tests/test_refhost.py``, which patches
``refhost._RefhostsFactory`` and ``refhost.HttpsResolver`` by name).
"""

import os
import time
from urllib.request import urlopen

from ...support.fileops import atomic_write_file
from ...support.paths import save_cache_path
from .models import Addon, Attributes, Host, Product, Version
from .resolvers import HttpsResolver, PathResolver, Resolver
from .store import Refhosts, RefhostsResolveFailedError, _RefhostsFactory

RefhostsFactory = _RefhostsFactory(
    {
        "https": HttpsResolver(
            time.time,
            os.stat,
            urlopen,
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
