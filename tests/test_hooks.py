import pytest
from mtui import hooks
from pathlib import Path
from unittest.mock import MagicMock, patch

class MockTestReport:
    def __repr__(self):
        return "<MockTestReport>"

def test_script_base():
    """
    Test Script base class
    """
    tr = MockTestReport()
    path = Path("/tmp/pre_script.sh")
    script = hooks.PreScript(tr, path)
    assert "pre script" in str(script)
    assert "<MockTestReport>" in repr(script)

def test_pre_script(monkeypatch):
    """
    Test PreScript
    """
    tr = MagicMock()
    tr.target_wd.return_value = "remote_path"
    tr.report_wd.return_value = "local_path"

    targets = MagicMock()

    path = Path("/tmp/pre_script.sh")
    script = hooks.PreScript(tr, path)
    script._run(targets)

    targets.sftp_put.assert_any_call(path, "remote_path")
    targets.run.assert_called_once()

@patch('subprocess.run')
def test_compare_script(mock_run):
    """
    Test CompareScript
    """
    tr = MagicMock()
    tr.report_wd.side_effect = lambda *args, **kwargs: Path("/".join(args))

    target = MagicMock()
    target.hostname = "test_host"

    path = Path("/tmp/compare_script.sh")
    script = hooks.CompareScript(tr, path)
    script._run_single_target(target)

    mock_run.assert_called_once()
