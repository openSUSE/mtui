from json import loads
from pathlib import Path

import pytest

__root__: Path = Path(__file__).parent / "fixtures"


@pytest.fixture
def log_txt() -> str:
    logfile = __root__ / "metadata" / "log"
    return logfile.read_text(errors="replace")


@pytest.fixture
def log_json():
    logfile = __root__ / "metadata" / "metadata.json"
    return loads(logfile.read_text())


@pytest.fixture(scope="session")
def log_install(tmp_path_factory) -> Path:
    data = "\n".join(
        ["Fake installog file", "", "one more line", "   end of fake install file"]
    )
    tgt = tmp_path_factory.mktemp("logfile") / "install.log"
    tgt.write_text(data)
    return tgt
