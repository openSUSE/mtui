from pathlib import Path
from typing import Any

from ..target.hostgroup import HostsGroup
from ..types import Product
from .testreport import TestReport


class NullTestReport(TestReport):
    @property
    def _type(self) -> str:
        return "No"

    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)
        self.path = Path.cwd() / "None"

    @property
    def id(self) -> str:
        return ""

    def __bool__(self) -> bool:
        return False

    def target_wd(self, *paths) -> Path:
        return self.config.target_tempdir.joinpath(*paths)  # type: ignore

    def _parser(self) -> dict[str, Any]:
        return {}

    def _update_repos_parser(self) -> dict[Product, str]:
        return {}

    def list_update_commands(self, targets: HostsGroup, display) -> None: ...
