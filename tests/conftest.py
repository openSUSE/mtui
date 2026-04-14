from json import loads
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mtui.types import HostLog, Package, Product
from mtui.types.rpmver import RPMVersion
from mtui.types.systems import System

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


# --- Shared mock fixtures ---


@pytest.fixture
def mock_config():
    """A reusable mock Config object with sane defaults."""
    config = MagicMock()
    config.session_user = "testuser"
    config.connection_timeout = 300
    config.template_dir = Path("/tmp/templates")
    config.smelt_api = "https://smelt.example.com/graphql"
    config.gitea_token = "test-token-123"
    config.location = "nuremberg"
    config.auto = False
    config.chdir_to_template_dir = False
    config.openqa_install_distri = "sle"
    config.openqa_install_logs = "install-logs.tar"
    config.install_logs = "install_logs"
    config.reports_url = "https://reports.example.com"
    return config


@pytest.fixture
def mock_system():
    """A reusable System object for tests."""
    base = Product("SLES", "15-SP5", "x86_64")
    return System(base)


@pytest.fixture
def mock_connection():
    """A mock Connection with common methods pre-configured."""
    conn = MagicMock()
    conn.hostname = "host1.example.com"
    conn.port = 22
    conn.timeout = 300
    conn.stdout = ""
    conn.stderr = ""
    conn.run.return_value = 0
    conn.is_active.return_value = True
    return conn


@pytest.fixture
def mock_target(mock_config, mock_connection, mock_system):
    """A Target-like mock with realistic attributes and methods."""
    from mtui.target import Target

    target = Target(mock_config, "host1.example.com")
    target.connection = mock_connection
    target._lock = MagicMock()
    target._lock.is_locked.return_value = False
    target._lock.is_mine.return_value = True
    target.system = mock_system
    target.transactional = False
    target.packages = {
        "bash": Package("bash"),
        "openssl": Package("openssl"),
    }
    target.packages["bash"].current = RPMVersion("5.1-1.1")
    target.packages["bash"].required = RPMVersion("5.1-1.2")
    target.packages["openssl"].current = RPMVersion("3.0.8-1.1")
    target.packages["openssl"].required = RPMVersion("3.0.8-1.2")
    return target


@pytest.fixture
def mock_target_pair(mock_config, mock_system):
    """Two mock targets suitable for HostsGroup testing."""
    from mtui.target import Target

    def make_target(hostname):
        t = Target(mock_config, hostname)
        conn = MagicMock()
        conn.hostname = hostname
        conn.port = 22
        conn.timeout = 300
        conn.stdout = ""
        conn.stderr = ""
        conn.run.return_value = 0
        conn.is_active.return_value = True
        t.connection = conn
        t._lock = MagicMock()
        t._lock.is_locked.return_value = False
        t._lock.is_mine.return_value = True
        t.system = mock_system
        t.transactional = False
        t.packages = {}
        t.out = HostLog()
        return t

    return make_target("host1.example.com"), make_target("host2.example.com")


@pytest.fixture
def mock_rrid():
    """A mock RequestReviewID."""
    rrid = MagicMock()
    rrid.maintenance_id = "12345"
    rrid.review_id = "67890"
    rrid.kind = "SLE"
    rrid.__str__ = lambda self: "SUSE:Maintenance:12345:67890"
    return rrid
