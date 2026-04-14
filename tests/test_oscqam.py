"""Tests for the mtui connector oscqam module."""

from unittest.mock import patch

import pytest

from mtui.connector.oscqam import OSC


@pytest.fixture
def osc(mock_config, mock_rrid):
    """Create an OSC instance with mock config and rrid."""
    return OSC(mock_config, mock_rrid)


class TestOSCInit:
    def test_init(self, osc, mock_config, mock_rrid):
        """Test OSC initialization."""
        assert osc.config is mock_config
        assert osc.rrid is mock_rrid


class TestOSCOperations:
    @patch("mtui.connector.oscqam.check_call")
    def test_approve(self, mock_check_call, osc):
        """Test approve builds correct command."""
        osc.approve(["qam-sle"])

        mock_check_call.assert_called_once()
        cmd = mock_check_call.call_args[0][0]
        assert cmd[0] == "osc"
        assert "approve" in cmd
        assert "-G" in cmd
        assert "qam-sle" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_assign(self, mock_check_call, osc):
        """Test assign builds correct command."""
        osc.assign(["qam-sle"])

        cmd = mock_check_call.call_args[0][0]
        assert "assign" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_unassign(self, mock_check_call, osc):
        """Test unassign builds correct command."""
        osc.unassign(["qam-sle"])

        cmd = mock_check_call.call_args[0][0]
        assert "unassign" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_reject_with_reason_and_message(self, mock_check_call, osc):
        """Test reject includes reason and message."""
        osc.reject(["qam-sle"], "bug found", "details here")

        cmd = mock_check_call.call_args[0][0]
        assert "reject" in cmd
        assert "-R" in cmd
        assert "bug found" in cmd
        assert "-M" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_comment(self, mock_check_call, osc):
        """Test comment builds correct command."""
        osc.comment("test comment")

        cmd = mock_check_call.call_args[0][0]
        assert "comment" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_multiple_groups(self, mock_check_call, osc):
        """Test command with multiple groups adds -G for each."""
        osc.approve(["qam-sle", "qam-kernel"])

        cmd = mock_check_call.call_args[0][0]
        g_indices = [i for i, x in enumerate(cmd) if x == "-G"]
        assert len(g_indices) == 2

    @patch("mtui.connector.oscqam.check_call")
    def test_empty_groups(self, mock_check_call, osc):
        """Test command with no groups omits -G."""
        osc.comment("hello")

        cmd = mock_check_call.call_args[0][0]
        assert "-G" not in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_skip_template_for_pi(self, mock_check_call, osc):
        """Test --skip-template is added for PI kind."""
        osc.rrid.kind = "PI"
        osc.approve(["qam-sle"])

        cmd = mock_check_call.call_args[0][0]
        assert "--skip-template" in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_no_skip_template_for_sle(self, mock_check_call, osc):
        """Test --skip-template is NOT added for SLE kind."""
        osc.rrid.kind = "SLE"
        osc.approve(["qam-sle"])

        cmd = mock_check_call.call_args[0][0]
        assert "--skip-template" not in cmd

    @patch("mtui.connector.oscqam.check_call")
    def test_calledprocesserror_logged(self, mock_check_call, osc):
        """Test CalledProcessError is caught and logged."""
        from subprocess import CalledProcessError

        mock_check_call.side_effect = CalledProcessError(1, "osc")
        osc.approve(["qam-sle"])  # should not raise

    @patch("mtui.connector.oscqam.check_call")
    def test_filenotfounderror_logged(self, mock_check_call, osc):
        """Test FileNotFoundError is caught and logged."""
        mock_check_call.side_effect = FileNotFoundError("osc not found")
        osc.approve(["qam-sle"])  # should not raise

    @patch("mtui.connector.oscqam.check_call")
    def test_api_url(self, mock_check_call, osc):
        """Test API URL is passed correctly."""
        osc.approve(["qam-sle"])

        cmd = mock_check_call.call_args[0][0]
        assert "-A" in cmd
        api_index = cmd.index("-A")
        assert cmd[api_index + 1] == "https://api.suse.de"
