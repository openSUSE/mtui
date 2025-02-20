from collections import UserList
from logging import getLogger
from pathlib import Path
from typing import Self

from ..utils import atomic_write_file

logger = getLogger("mtui.types.filelist")


class FileList(UserList):
    __slots__ = ["_hash", "_file", "data"]

    @classmethod
    def load(cls, path: Path | str, *args, **kwds) -> Self:
        if isinstance(path, str):
            path = Path(path)
        instance = super().__new__(cls, *args, **kwds)
        instance._file = path  # type: ignore
        with path.open(mode="r", encoding="utf-8", errors="replace") as text:
            instance.data = text.readlines()  # type: ignore

        instance._hash = hash("".join(instance.data))  # type: ignore
        return instance

    def read(self) -> None:
        pass

    def write(self) -> None:
        atomic_write_file("".join(self.data), self._file)  # type: ignore

    def __enter__(self, *args) -> Self:
        return self

    def __exit__(self, *args) -> None:
        if self._hash != hash("".join(self.data)):  # type: ignore
            logger.debug("Writing template to %s", self._file)  # type: ignore
            self.write()
