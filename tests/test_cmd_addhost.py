"""Tests for the `add_host` command."""

from __future__ import annotations

import sys
from argparse import Namespace
from io import StringIO
from unittest.mock import MagicMock, patch

from mtui.commands.addhost import AddHost
from mtui.types import Workflow


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.workflow = Workflow.MANUAL
    p.metadata.testplatforms = ["base=sles(major=15);arch=[x86_64]"]
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_add_host_argparser_accepts_target_and_keep_mode():
    """The argument parser wires up -t/--target and -k/--keep-mode."""
    ns = AddHost.parse_args("-t h1 -t h2 -k", sys)
    assert ns.target == ["h1", "h2"]
    assert ns.keep_mode is True

    default = AddHost.parse_args("", sys)
    assert default.target is None
    assert default.keep_mode is False


def test_add_host_complete_offers_flags():
    """Tab completion offers -t and the new -k/--keep-mode flag."""
    out = AddHost.complete({"hosts": []}, "", "add_host ", 9, 9)
    assert "--keep-mode" in out
    assert "--target" in out


def test_add_host_with_explicit_targets_submits_per_host(mock_config):
    prompt = _prompt()
    args = Namespace(target=["h1", "h2"], keep_mode=False)

    with (
        patch("mtui.commands.addhost.ContextExecutor") as tpe,
        patch("mtui.commands.addhost.concurrent.futures.wait") as wait,
    ):
        executor = MagicMock()
        tpe.return_value.__enter__.return_value = executor
        AddHost(args, mock_config, MagicMock(), prompt)()
        wait.assert_called_once()

    assert executor.submit.call_count == 2
    submitted_args = [c.args[1] for c in executor.submit.call_args_list]
    assert submitted_args == ["h1", "h2"]
    # the callable passed must be the metadata.add_target bound method
    for c in executor.submit.call_args_list:
        assert c.args[0] is prompt.metadata.add_target


def test_add_host_without_targets_uses_testplatforms(mock_config):
    prompt = _prompt()
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.refhosts_from_tp.assert_called_once_with(
        "base=sles(major=15);arch=[x86_64]"
    )
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_in_automatic_mode_switches_to_manual(mock_config):
    """Running add_host while in automatic mode switches to the manual workflow."""
    prompt = _prompt()
    prompt.metadata.workflow = Workflow.AUTO
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    assert prompt.metadata.workflow is Workflow.MANUAL
    # Prompt indicator refreshed (drops the "-auto" marker).
    prompt.set_prompt.assert_called_once_with()
    # The hosts are still added.
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_keep_mode_stays_automatic(mock_config):
    """--keep-mode leaves automatic mode untouched even though a host is added."""
    prompt = _prompt()
    prompt.metadata.workflow = Workflow.AUTO
    args = Namespace(target=None, keep_mode=True)

    AddHost(args, mock_config, MagicMock(), prompt)()

    assert prompt.metadata.workflow is Workflow.AUTO  # still automatic
    prompt.set_prompt.assert_not_called()
    # The hosts are still added.
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_does_not_echo_product_warnings_to_stdout(mock_config):
    """add_host no longer re-echoes product-drift warnings to stdout.

    Drift is reported once, via ``logger.warning`` in
    ``TestReport._verify_target_products`` (covered in test_testreport.py);
    under MCP the session tees those records into the reply
    (test_mcp_session.py). The old ``println(f"WARNING: ...")`` echo --
    which duplicated the logger output in the REPL -- is gone.
    """
    prompt = _prompt()
    prompt.metadata.targets = {}
    prompt.metadata.product_warnings = {"h1": ["arch 'x86_64' != 'aarch64' (metadata)"]}
    args = Namespace(target=["h1"], keep_mode=False)

    fake_sys = MagicMock()
    fake_sys.stdout = StringIO()

    def _connect(*_a, **_k):
        prompt.metadata.targets["h1"] = MagicMock()

    with (
        patch("mtui.commands.addhost.ContextExecutor"),
        patch("mtui.commands.addhost.concurrent.futures.wait", side_effect=_connect),
    ):
        AddHost(args, mock_config, fake_sys, prompt)()

    output = fake_sys.stdout.getvalue()
    assert "WARNING:" not in output
    assert output == ""


def test_add_host_in_manual_mode_does_not_switch(mock_config):
    """In manual mode add_host leaves the workflow untouched."""
    prompt = _prompt()
    prompt.metadata.workflow = Workflow.MANUAL
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    prompt.set_prompt.assert_not_called()


def test_add_host_is_fanout():
    """add_host fans out across every loaded template by default."""
    assert AddHost.scope == "fanout"


def test_add_host_accepts_template_flag():
    ns = AddHost.parse_args("-T SUSE:Maintenance:1:1", sys)
    assert ns.template == "SUSE:Maintenance:1:1"
    assert ns.all_templates is False


def test_add_host_accepts_all_templates_flag():
    ns = AddHost.parse_args("--all-templates", sys)
    assert ns.all_templates is True
    assert ns.template is None


def test_add_host_defaults_have_template_flags():
    default = AddHost.parse_args("", sys)
    assert default.template is None
    assert default.all_templates is False


def test_add_host_complete_offers_template_rrids():
    templates = MagicMock()
    templates.rrids.return_value = ["SUSE:Maintenance:1:1"]
    out = AddHost.complete({"hosts": [], "templates": templates}, "", "add_host ", 9, 9)
    assert "SUSE:Maintenance:1:1" in out
