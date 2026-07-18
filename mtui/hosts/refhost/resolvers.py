"""Strategies for producing a :class:`Refhosts` from a YAML source.

Each :class:`Resolver` knows one way to obtain ``refhosts.yml`` —
:class:`PathResolver` from a local file, :class:`HttpsResolver` from a
cached HTTPS download. The bound :data:`mtui.hosts.refhost.RefhostsFactory`
singleton tries them in the order named by ``config.refhosts_resolvers``.
"""

import errno
from abc import ABC, abstractmethod
from collections.abc import Callable
from logging import getLogger
from pathlib import Path

from ...support.http import resolve_verify
from .store import Refhosts, load_refhosts

logger = getLogger("mtui.refhost")


class Resolver(ABC):
    """A strategy for producing a :class:`Refhosts` instance from a source."""

    @abstractmethod
    def resolve(self, config) -> Refhosts:
        """Return a :class:`Refhosts` built from this resolver's source."""


class PathResolver(Resolver):
    """Resolve refhosts from a local file at ``config.refhosts_path``."""

    def __init__(
        self, refhosts_factory: Callable[[Path], Refhosts] = load_refhosts
    ) -> None:
        self.refhosts_factory = refhosts_factory

    def resolve(self, config) -> Refhosts:
        return self.refhosts_factory(config.refhosts_path)


class HttpsResolver(Resolver):
    """Resolve refhosts from an HTTPS URL, with on-disk caching."""

    def __init__(
        self,
        time_now_getter,
        statter,
        urlopener,
        file_writer,
        cache_path: Path,
        refhosts_factory: Callable[[Path], Refhosts] = load_refhosts,
    ) -> None:
        """Initialize the resolver.

        Args:
            time_now_getter: Callable returning the current epoch time.
            statter: Callable returning file stats (``os.stat``).
            urlopener: Callable ``(uri, verify) -> response`` returning an
                object with a ``.read()`` method yielding the payload
                bytes. ``verify`` is the :data:`~mtui.support.http.VerifyPolicy`
                to apply to the request.
            file_writer: Callable ``(bytes, path) -> None`` to persist
                the downloaded payload.
            cache_path: On-disk cache path for the downloaded YAML.
            refhosts_factory: Factory for the :class:`Refhosts` instance.

        """
        self._time_now = time_now_getter
        self._stat = statter
        self._urlopen = urlopener
        self._write_file = file_writer
        self.cache_path = cache_path
        self.refhosts_factory = refhosts_factory

    def resolve(self, config) -> Refhosts:
        self._refresh_if_needed(config)
        return self.refhosts_factory(self.cache_path)

    def _refresh_if_needed(self, config) -> None:
        if self._is_refresh_needed(config.refhosts_https_expiration):
            # refhosts.yml is served from an internal SUSE host; verify by
            # default but let the global [mtui] ssl_verify policy override.
            verify = resolve_verify(True, config.ssl_verify)
            self._refresh(config.refhosts_https_uri, verify)

    def _is_refresh_needed(self, expiration: int) -> bool:
        try:
            statinfo = self._stat(self.cache_path)
        except OSError as e:
            if e.errno == errno.ENOENT:
                return True
            raise

        return self._time_now() - statinfo.st_mtime > expiration

    def _refresh(self, uri: str, verify) -> None:
        self._write_file(self._urlopen(uri, verify).read(), self.cache_path)
