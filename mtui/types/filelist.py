"""A list-like object that can be loaded from and saved to a file."""

from collections import UserList
from logging import getLogger
from pathlib import Path
from typing import Self

from ..utils import atomic_write_file

logger = getLogger("mtui.types.filelist")


class FileList(UserList):
    """A list-like object that can be loaded from and saved to a file."""

    __slots__ = ["_hash", "_file", "data"]

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
        instance._file = path  # type: ignore
        with path.open(mode="r", encoding="utf-8", errors="replace") as text:
            instance.data = text.readlines()  # type: ignore

        instance._hash = hash("".join(instance.data))  # type: ignore
        return instance

    def read(self) -> None:
        """Does nothing."""
        pass

    def write(self) -> None:
        """Writes the `FileList` to a file."""
        atomic_write_file("".join(self.data), self._file)  # type: ignore

    def __enter__(self, *args) -> Self:
        """Enters a context manager.

        Returns:
            The `FileList` instance.
        """
        return self

    def __exit__(self, *args) -> None:
        """Exits a context manager, writing the file if it has been modified."""
        if self._hash != hash("".join(self.data)):  # type: ignore
            logger.debug("Writing template to %s", self._file)  # type: ignore
            self.write()
