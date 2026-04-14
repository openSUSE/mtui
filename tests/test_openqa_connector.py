"""Tests for the mtui connector openqa modules."""

from unittest.mock import MagicMock, patch

import pytest

from mtui.connector.openqa.standard import AutoOpenQA


@pytest.fixture
def mock_smelt():
    """Create a mock SMELT connector."""
    smelt = MagicMock()
    smelt.get_incident_name.return_value = "bash"
    return smelt


@pytest.fixture
def auto_oqa(mock_config, mock_rrid, mock_smelt):
    """Create an AutoOpenQA instance with mocked dependencies."""
    with patch("mtui.connector.openqa.base.oqa"):
        oqa = AutoOpenQA(
            mock_config, "https://openqa.example.com", mock_smelt, mock_rrid
        )
    return oqa


# --- AutoOpenQA._has_passed_install_jobs ---


class TestHasPassedInstallJobs:
    def test_none_jobs_returns_false(self, auto_oqa):
        """Test None jobs returns False."""
        assert auto_oqa._has_passed_install_jobs(None) is False

    def test_empty_jobs_returns_true(self, auto_oqa):
        """Test empty jobs list returns True (vacuously)."""
        assert auto_oqa._has_passed_install_jobs([]) is True

    def test_all_install_jobs_passed(self, auto_oqa):
        """Test all install jobs passed returns True."""
        jobs = [
            {"test": "qam-incidentinstall", "result": "passed"},
            {"test": "qam-incidentinstall-ha", "result": "softfailed"},
        ]
        assert auto_oqa._has_passed_install_jobs(jobs) is True

    def test_failed_install_job(self, auto_oqa):
        """Test failed install job returns False."""
        jobs = [
            {"test": "qam-incidentinstall", "result": "failed"},
        ]
        assert auto_oqa._has_passed_install_jobs(jobs) is False

    def test_incomplete_install_job(self, auto_oqa):
        """Test incomplete install job returns False."""
        jobs = [
            {"test": "qam-incidentinstall", "result": "incomplete"},
        ]
        assert auto_oqa._has_passed_install_jobs(jobs) is False

    def test_non_install_jobs_ignored(self, auto_oqa):
        """Test non-install jobs are ignored."""
        jobs = [
            {"test": "qam-somethertest", "result": "failed"},
        ]
        assert auto_oqa._has_passed_install_jobs(jobs) is True


# --- AutoOpenQA._pretty_print ---


class TestPrettyPrint:
    def test_empty_jobs(self, auto_oqa):
        """Test pretty_print with no jobs returns empty list."""
        result = auto_oqa._pretty_print(None)
        assert result == []

    def test_with_jobs(self, auto_oqa):
        """Test pretty_print formats job results."""
        jobs = [
            {
                "test": "qam-incidentinstall",
                "result": "passed",
                "settings": {
                    "FLAVOR": "Server-DVD",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                },
                "modules": [],
            }
        ]
        result = auto_oqa._pretty_print(jobs)

        assert len(result) > 0
        assert any("openQA" in line for line in result)
        assert any("x86_64" in line for line in result)

    def test_with_failed_modules(self, auto_oqa):
        """Test pretty_print shows failed modules."""
        jobs = [
            {
                "test": "qam-incidentinstall",
                "result": "failed",
                "settings": {
                    "FLAVOR": "Server-DVD",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                },
                "modules": [
                    {"name": "install", "category": "install", "result": "passed"},
                    {"name": "reboot", "category": "boot", "result": "failed"},
                ],
            }
        ]
        result = auto_oqa._pretty_print(jobs)

        assert any("Failed modules" in line for line in result)
        assert any("reboot" in line for line in result)


# --- AutoOpenQA._get_logs_url ---


class TestGetLogsUrl:
    def test_no_jobs(self, auto_oqa):
        """Test _get_logs_url with no jobs returns None."""
        result = auto_oqa._get_logs_url(None)
        assert result is None

    def test_empty_jobs(self, auto_oqa):
        """Test _get_logs_url with empty list returns None."""
        result = auto_oqa._get_logs_url([])
        assert result is None

    def test_with_install_jobs(self, auto_oqa):
        """Test _get_logs_url extracts URLs from install jobs."""
        jobs = [
            {
                "id": 123,
                "test": "qam-incidentinstall",
                "result": "passed",
                "settings": {
                    "HDD_1": "SLES-15-SP5-x86_64-Build1234.qcow2",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                },
            },
            {
                "id": 456,
                "test": "qam-othertest",
                "result": "passed",
                "settings": {
                    "HDD_1": "SLES-15-SP5-x86_64-Build1234.qcow2",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                },
            },
        ]
        result = auto_oqa._get_logs_url(jobs)

        # Only install jobs should be included
        assert len(result) == 1
        assert "123" in result[0].url


# --- AutoOpenQA.run ---


class TestAutoOpenQARun:
    def test_run_with_passed_jobs(self, auto_oqa):
        """Test run() with passing install jobs."""
        mock_jobs = [
            {
                "id": 1,
                "test": "qam-incidentinstall",
                "result": "passed",
                "settings": {
                    "FLAVOR": "Server-DVD",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                    "HDD_1": "SLES-15-SP5.qcow2",
                },
                "modules": [],
            }
        ]
        auto_oqa.client = MagicMock()
        auto_oqa.client.openqa_request.return_value = {"jobs": mock_jobs}

        result = auto_oqa.run()

        assert result is auto_oqa
        assert auto_oqa.results is not None
        assert len(auto_oqa.pp) > 0

    def test_run_with_failed_jobs(self, auto_oqa):
        """Test run() with failing install jobs sets results to None."""
        mock_jobs = [
            {
                "id": 1,
                "test": "qam-incidentinstall",
                "result": "failed",
                "settings": {
                    "FLAVOR": "Server-DVD",
                    "ARCH": "x86_64",
                    "VERSION": "15-SP5",
                    "HDD_1": "SLES-15-SP5.qcow2",
                },
                "modules": [],
            }
        ]
        auto_oqa.client = MagicMock()
        auto_oqa.client.openqa_request.return_value = {"jobs": mock_jobs}

        result = auto_oqa.run()

        assert result is auto_oqa
        assert auto_oqa.results is None


# --- AutoOpenQA.__bool__ ---


class TestAutoOpenQABool:
    def test_bool_false_when_empty(self, auto_oqa):
        """Test bool is False when no results or pp."""
        auto_oqa.results = None
        auto_oqa.pp = []
        assert bool(auto_oqa) is False

    def test_bool_true_when_pp(self, auto_oqa):
        """Test bool is True when pp has content."""
        auto_oqa.results = None
        auto_oqa.pp = ["something"]
        assert bool(auto_oqa) is True

    def test_bool_true_when_results(self, auto_oqa):
        """Test bool is True when results has content."""
        auto_oqa.results = [MagicMock()]
        auto_oqa.pp = []
        assert bool(auto_oqa) is True
