from json import loads
from pathlib import Path

import pytest


__root__ = Path(__file__).parent / "fixtures"


@pytest.fixture
def log_txt():
    logfile = __root__ / "metadata" / "log"
    return logfile.read_text(errors="replace")


@pytest.fixture
def log_json():
    logfile = __root__ / "metadata" / "metadata.json"
    return loads(logfile.read_text())
