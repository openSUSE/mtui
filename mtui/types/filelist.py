from collections import UserList
from logging import getLogger
from pathlib import Path

from ..utils import atomic_write_file

logger = getLogger("mtui.types.filelist")


class FileList(UserList):
    __slots__ = ["_hash", "_file", "data"]

    @classmethod
    def load(cls, path, *args, **kwds):
        if isinstance(path, str):
            path = Path(path)
        instance = super().__new__(cls, *args, **kwds)
        instance._file = path
        with path.open(mode="r", encoding="utf-8", errors="replace") as text:
            instance.data = text.readlines()

        instance._hash = hash("".join(instance.data))
        return instance

    def read(self):
        pass

    def write(self):
        atomic_write_file("".join(self.data), self._file)

    def __enter__(self, *args):
        return self

    def __exit__(self, *args):
        if self._hash != hash("".join(self.data)):
            logger.debug(f"Writing template to {self._file}")
            self.write()
