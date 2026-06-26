"""Tests for the ``regenerate`` command (TeReGen-backed template regeneration)."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.regenerate import Regenerate
from mtui.data_sources.teregen import RegenOutcome
from mtui.types.enums import Workflow


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.rrid = "SUSE:SLFO:1.2:5444"
    p.metadata.workflow = Workflow.AUTO
    return p


def _sysmock() -> MagicMock:
    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def _args(**kw) -> Namespace:
    base = {
        "force": False,
        "ignore_inconsistent": False,
        "no_wait": False,
        "template": None,
        "all_templates": False,
    }
    base.update(kw)
    return Namespace(**base)


def test_regenerate_enqueues_waits_and_reloads(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with (
        patch("mtui.commands.regenerate.TeReGen") as teregen_cls,
        patch("mtui.commands.regenerate.AutoOBSUpdateID") as update_cls,
    ):
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(ok=True, job=42)

        Regenerate(_args(), mock_config, sysmock, prompt)()

    teregen.regenerate_and_wait.assert_called_once()
    # The spinner stop-predicate is forwarded as the wait's cancellation hook.
    assert callable(teregen.regenerate_and_wait.call_args.kwargs["should_stop"])
    prompt.load_update.assert_called_once()
    update_cls.assert_called_once_with("SUSE:SLFO:1.2:5444")
    out = sysmock.stdout.getvalue()
    assert "enqueued" in out
    assert "reloading" in out


def test_regenerate_no_wait_skips_reload(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate.return_value = {"id": "SUSE:SLFO:1.2:5444", "job": 7}

        Regenerate(_args(no_wait=True), mock_config, sysmock, prompt)()

    # --no-wait enqueues only; it never enters the wait path.
    teregen.regenerate.assert_called_once()
    teregen.regenerate_and_wait.assert_not_called()
    prompt.load_update.assert_not_called()
    assert "Not waiting" in sysmock.stdout.getvalue()


def test_regenerate_reports_refusal(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(
            ok=False, error="template already edited"
        )

        Regenerate(_args(), mock_config, sysmock, prompt)()

    prompt.load_update.assert_not_called()
    out = sysmock.stdout.getvalue()
    assert "refused" in out
    assert "already edited" in out


def test_regenerate_refusal_hint_skips_flags_already_set(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(
            ok=False, error="inconsistent"
        )

        # --force already set; only --ignore-inconsistent should be suggested.
        Regenerate(_args(force=True), mock_config, sysmock, prompt)()

    out = sysmock.stdout.getvalue()
    assert "--ignore-inconsistent" in out
    assert "--force" not in out


def test_regenerate_refusal_hint_omitted_when_both_flags_set(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(ok=False, error="nope")

        Regenerate(
            _args(force=True, ignore_inconsistent=True), mock_config, sysmock, prompt
        )()

    out = sysmock.stdout.getvalue()
    assert "Retry with" not in out


def test_regenerate_reports_unreachable(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(
            ok=False, unreachable=True
        )

        Regenerate(_args(), mock_config, sysmock, prompt)()

    prompt.load_update.assert_not_called()
    assert "unreachable" in sysmock.stdout.getvalue()


def test_regenerate_unfinished_does_not_reload(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.regenerate.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.regenerate_and_wait.return_value = RegenOutcome(
            ok=False, state="failed", minion_error="boom", job=9
        )

        Regenerate(_args(), mock_config, sysmock, prompt)()

    prompt.load_update.assert_not_called()
    out = sysmock.stdout.getvalue()
    assert "did not finish" in out
    assert "boom" in out
