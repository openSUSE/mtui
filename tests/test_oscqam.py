"""Tests for the mtui connector oscqam module."""

import logging
import shlex
from subprocess import (
    DEVNULL,
    CalledProcessError,
    CompletedProcess,
    TimeoutExpired,
)
from unittest.mock import patch

import pytest

from mtui.data_sources.oscqam import OSC
from mtui.types import RequestKind

_LOGGER = "mtui.connector.oscqam"


@pytest.fixture
def osc(mock_config, mock_rrid):
    """Create an OSC instance with mock config and rrid."""
    return OSC(mock_config, mock_rrid)


@pytest.fixture
def mock_run():
    """Patch ``run`` in oscqam with a clean, empty-output success by default.

    Tests that need a failure set ``mock_run.side_effect`` to the relevant
    exception; the empty ``stdout``/``stderr`` keeps the success path from
    blowing up on the captured-output logging.
    """
    with patch("mtui.data_sources.oscqam.run") as m:
        m.return_value = CompletedProcess(
            args=["osc"], returncode=0, stdout="", stderr=""
        )
        yield m


class TestOSCInit:
    def test_init(self, osc, mock_config, mock_rrid):
        """Test OSC initialization."""
        assert osc.config is mock_config
        assert osc.rrid is mock_rrid


class TestOSCCommandBuilding:
    def test_approve(self, mock_run, osc):
        """Test approve builds correct command."""
        osc.approve(["qam-sle"])

        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        assert cmd[0] == "osc"
        assert "approve" in cmd
        assert "-G" in cmd
        assert "qam-sle" in cmd

    def test_assign(self, mock_run, osc):
        """Test assign builds correct command."""
        osc.assign(["qam-sle"])

        cmd = mock_run.call_args[0][0]
        assert "assign" in cmd

    def test_unassign(self, mock_run, osc):
        """Test unassign builds correct command."""
        osc.unassign(["qam-sle"])

        cmd = mock_run.call_args[0][0]
        assert "unassign" in cmd

    def test_reject_with_reason_and_message(self, mock_run, osc):
        """Test reject includes reason and message."""
        osc.reject(["qam-sle"], "bug found", "details here")

        cmd = mock_run.call_args[0][0]
        assert "reject" in cmd
        assert "-R" in cmd
        assert "bug found" in cmd
        assert "-M" in cmd

    def test_comment(self, mock_run, osc):
        """Test comment builds correct command."""
        osc.comment("test comment")

        cmd = mock_run.call_args[0][0]
        assert "comment" in cmd

    def test_multiple_groups(self, mock_run, osc):
        """Test command with multiple groups adds -G for each."""
        osc.approve(["qam-sle", "qam-kernel"])

        cmd = mock_run.call_args[0][0]
        g_indices = [i for i, x in enumerate(cmd) if x == "-G"]
        assert len(g_indices) == 2

    def test_empty_groups(self, mock_run, osc):
        """Test command with no groups omits -G."""
        osc.comment("hello")

        cmd = mock_run.call_args[0][0]
        assert "-G" not in cmd

    def test_skip_template_for_pi(self, mock_run, osc):
        """Test --skip-template is added for PI kind."""
        osc.rrid.kind = RequestKind.PI
        osc.approve(["qam-sle"])

        cmd = mock_run.call_args[0][0]
        assert "--skip-template" in cmd

    def test_no_skip_template_for_sle(self, mock_run, osc):
        """Test --skip-template is NOT added for non-PI/non-SLFO kinds."""
        osc.rrid.kind = RequestKind.MAINTENANCE
        osc.approve(["qam-sle"])

        cmd = mock_run.call_args[0][0]
        assert "--skip-template" not in cmd

    def test_api_url(self, mock_run, osc):
        """Test API URL is passed correctly."""
        osc.approve(["qam-sle"])

        cmd = mock_run.call_args[0][0]
        assert "-A" in cmd
        api_index = cmd.index("-A")
        assert cmd[api_index + 1] == "https://api.suse.de"


# Payloads that shell-quoting would corrupt but argv-mode execution must carry
# byte-for-byte: spaces, an apostrophe (the worst shlex.quote case), embedded
# double + mixed quotes, non-ASCII, and a value that looks like an option.
_VERBATIM_PAYLOADS = [
    "does not build",
    "won't build",
    'has "double" quotes',
    "mixes ' and \" quotes",
    "naïve café ☃",
    "--looks-like-a-flag",
]


class TestOSCArgvVerbatim:
    """Message/comment must reach osc verbatim, never shlex-quoted.

    The command is an argv list run without ``shell=True``; adding
    ``shlex.quote`` would ship literal quote characters to osc and
    corrupt the recorded rejection message / comment.
    """

    def _argv(self, mock_run):
        return mock_run.call_args[0][0]

    @pytest.mark.parametrize("message", _VERBATIM_PAYLOADS)
    def test_reject_message_is_verbatim_argv_element(self, mock_run, osc, message):
        """The reject message appears as one raw argv element after -M."""
        osc.reject(["qam-sle"], "bug found", message)

        cmd = self._argv(mock_run)
        assert message in cmd  # exact string, not a quoted variant
        assert cmd[cmd.index("-M") + 1] == message
        # no shell-escaping artifacts leaked into any argv element
        assert not any(x.startswith("'") for x in cmd)

    @pytest.mark.parametrize("comment", _VERBATIM_PAYLOADS)
    def test_comment_is_verbatim_argv_element(self, mock_run, osc, comment):
        """The comment appears as one raw argv element (last position)."""
        osc.comment(comment)

        cmd = self._argv(mock_run)
        assert comment in cmd
        assert cmd[-1] == comment
        assert not any(x.startswith("'") for x in cmd)

    def test_debug_log_still_renders_safely(self, mock_run, osc, caplog):
        """shlex_join quotes for the debug log only, not the argv itself."""
        with caplog.at_level(logging.DEBUG, logger=_LOGGER):
            osc.reject(["qam-sle"], "bug found", "won't build")
        cmd = self._argv(mock_run)
        # The real argv element stays raw (verbatim, no shell escaping)...
        assert cmd[cmd.index("-M") + 1] == "won't build"
        # ...while the debug log renders the whole command shlex-quoted for
        # display: the apostrophe message shows up escaped, never verbatim.
        assert f"Executing command: {shlex.join(cmd)}" in caplog.text
        assert shlex.quote("won't build") in caplog.text
        assert "won't build" not in caplog.text


class TestOSCInvocation:
    def test_stdin_detached_captured_and_timed(self, mock_run, osc):
        """osc must not inherit our stdin, must capture output, and be time-capped.

        Detaching stdin (the MCP JSON-RPC pipe) keeps an interactive osc prompt
        from deadlocking the single-threaded server; capturing output lets a
        failure report osc's real reason; the timeout is the backstop.
        """
        osc.approve(["qam-sle"])

        kwargs = mock_run.call_args.kwargs
        assert kwargs.get("stdin") is DEVNULL
        assert kwargs.get("capture_output") is True
        assert kwargs.get("text") is True
        assert kwargs.get("check") is True
        assert kwargs.get("timeout")


class TestOSCOutcome:
    def test_returns_true_on_success(self, mock_run, osc):
        """A clean osc exit reports success to the caller."""
        assert osc.approve(["qam-sle"]) is True

    def test_returns_false_on_calledprocesserror(self, mock_run, osc):
        """A non-zero osc exit reports failure to the caller (no raise)."""
        mock_run.side_effect = CalledProcessError(1, "osc", stderr="boom")
        assert osc.approve(["qam-sle"]) is False

    def test_returns_false_on_timeout(self, mock_run, osc):
        """A timed-out osc reports failure to the caller (no raise)."""
        mock_run.side_effect = TimeoutExpired("osc", 180)
        assert osc.approve(["qam-sle"]) is False

    def test_returns_false_when_osc_missing(self, mock_run, osc):
        """A missing osc binary reports failure to the caller (no raise)."""
        mock_run.side_effect = FileNotFoundError("osc not found")
        assert osc.approve(["qam-sle"]) is False


class TestOSCErrorReporting:
    def test_failure_surfaces_osc_stderr(self, mock_run, osc, caplog):
        """The captured osc stderr is logged so the caller learns *why*."""
        mock_run.side_effect = CalledProcessError(
            1, "osc", stderr="Error: request 414975 already accepted"
        )
        with caplog.at_level(logging.ERROR, logger=_LOGGER):
            osc.approve([])  # no group -> isolates the stderr-surfacing path
        assert "already accepted" in caplog.text

    def test_group_failure_adds_headless_hint(self, mock_run, osc, caplog):
        """A failed -G call hints to re-run without -G (the interactive trap)."""
        mock_run.side_effect = CalledProcessError(1, "osc", stderr="EOFError")
        with caplog.at_level(logging.ERROR, logger=_LOGGER):
            osc.approve(["qam-sle"])
        assert "without -G" in caplog.text

    def test_no_group_failure_omits_hint(self, mock_run, osc, caplog):
        """A failure without -G must not emit the -G hint."""
        mock_run.side_effect = CalledProcessError(1, "osc", stderr="nope")
        with caplog.at_level(logging.ERROR, logger=_LOGGER):
            osc.comment("hi")
        assert "without -G" not in caplog.text

    def test_success_logs_osc_stdout(self, mock_run, osc, caplog):
        """A clean run logs osc's confirmation output."""
        mock_run.return_value = CompletedProcess(
            args=["osc"],
            returncode=0,
            stdout="Approving 414975 for Martin Pluskal (qam-sle).",
            stderr="",
        )
        with caplog.at_level(logging.INFO, logger=_LOGGER):
            assert osc.approve([]) is True
        assert "Approving 414975" in caplog.text
