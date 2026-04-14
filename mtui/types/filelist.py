"""A list-like object that can be loaded from and saved to a file."""

from collections import UserList
from logging import getLogger
from pathlib import Path
from typing import Self

from ..utils import atomic_write_file

logger = getLogger("mtui.types.filelist")


class FileList(UserList):
    """A list-like object that can be loaded from and saved to a file."""

    __slots__ = ["_file", "_hash", "data"]

    _file: Path
    _hash: int

    @classmethod
    def load(cls, path: Path | str, *args, **kwds) -> Self:
        """Loads a `FileList` from a file.

        Args:
            path: The path to the file to load.
            *args: Additional arguments to pass to the constructor.
            **kwds: Additional keyword arguments to pass to the constructor.

        Returns:
            A `FileList` instance.

        """
        if isinstance(path, str):
            path = Path(path)
        instance = super().__new__(cls, *args, **kwds)
        instance._file = path
        with path.open(mode="r", encoding="utf-8", errors="replace") as text:
            instance.data = text.readlines()

        instance._hash = hash("".join(instance.data))
        return instance

    def read(self) -> None:
        """Does nothing."""

    def write(self) -> None:
        """Writes the `FileList` to a file."""
        atomic_write_file("".join(self.data), self._file)

    def __enter__(self, *args) -> Self:
        """Enters a context manager.

        Returns:
            The `FileList` instance.

        """
        return self

    def __exit__(self, *args) -> None:
        """Exits a context manager, writing the file if it has been modified."""
        if self._hash != hash("".join(self.data)):
            logger.debug("Writing template to %s", self._file)
            self.write()
