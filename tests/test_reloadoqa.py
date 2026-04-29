"""Tests for the `reload_openqa` command.

Includes a regression test for a previous bug where the kernel branch
attempted to append the baremetal connector to a non-existent
``self.metadata.openqa["baremetal"]`` key, raising ``KeyError`` on the
first reload in kernel workflow.
"""

from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.reloadoqa import ReloadOpenQA
from mtui.types import OpenQAResults, RequestReviewID


def _build_prompt() -> MagicMock:
    """Build a prompt mock with truthy metadata and an empty OpenQAResults."""
    prompt = MagicMock()
    # `requires_update` checks bool(self.metadata) -- ensure truthy
    prompt.metadata.__bool__ = lambda self: True
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = OpenQAResults()
    prompt.display = MagicMock()
    prompt.targets = MagicMock()
    return prompt


def test_reload_openqa_kernel_first_call_appends_both_to_kernel_list(mock_config):
    """Regression: previously raised KeyError on missing 'baremetal' key.

    Both the regular and the baremetal `KernelOpenQA` instances must be
    appended to the same `kernel` list (matches what
    `KernelOBSUpdateID.make_testreport` does).
    """
    prompt = _build_prompt()
    mock_config.kernel = True
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.openqa_instance_baremetal = "https://baremetal.example.com"
    args = Namespace()

    with (
        patch("mtui.commands.reloadoqa.KernelOpenQA") as kernel_oqa,
        patch("mtui.commands.reloadoqa.DashboardAutoOpenQA") as dashboard,
    ):
        # Each instantiation returns a distinct mock so we can tell them apart
        kernel_oqa.return_value.run.side_effect = lambda: MagicMock(
            name="kernel_run_result"
        )
        dashboard.return_value.run.return_value = MagicMock(name="auto_run_result")

        ReloadOpenQA(args, mock_config, MagicMock(), prompt)()

    # Two KernelOpenQA constructions, with the two distinct instances.
    assert kernel_oqa.call_count == 2
    instance_urls = [call.args[1] for call in kernel_oqa.call_args_list]
    assert instance_urls == [
        mock_config.openqa_instance,
        mock_config.openqa_instance_baremetal,
    ]
    # Both end up in the kernel list (the bugfix); no separate "baremetal" key.
    assert len(prompt.metadata.openqa.kernel) == 2

    # The auto branch also ran since auto was None.
    dashboard.assert_called_once()
    assert prompt.metadata.openqa.auto is dashboard.return_value.run.return_value


def test_reload_openqa_kernel_refresh_calls_run_on_existing(mock_config):
    """When kernel results already exist, .run() is called on each."""
    prompt = _build_prompt()
    existing_a = MagicMock(name="existing_a")
    existing_b = MagicMock(name="existing_b")
    prompt.metadata.openqa.kernel = [existing_a, existing_b]
    prompt.metadata.openqa.auto = MagicMock(name="existing_auto")
    mock_config.kernel = True
    args = Namespace()

    with (
        patch("mtui.commands.reloadoqa.KernelOpenQA") as kernel_oqa,
        patch("mtui.commands.reloadoqa.DashboardAutoOpenQA") as dashboard,
    ):
        ReloadOpenQA(args, mock_config, MagicMock(), prompt)()

    # No new kernel/auto connectors built; existing ones refreshed in place.
    kernel_oqa.assert_not_called()
    dashboard.assert_not_called()
    existing_a.run.assert_called_once()
    existing_b.run.assert_called_once()
    prompt.metadata.openqa.auto.run.assert_called_once()


def test_reload_openqa_auto_only_when_kernel_disabled(mock_config):
    """With kernel disabled, only the auto branch runs."""
    prompt = _build_prompt()
    mock_config.kernel = False
    args = Namespace()

    with (
        patch("mtui.commands.reloadoqa.KernelOpenQA") as kernel_oqa,
        patch("mtui.commands.reloadoqa.DashboardAutoOpenQA") as dashboard,
    ):
        dashboard.return_value.run.return_value = MagicMock(name="auto_run_result")
        ReloadOpenQA(args, mock_config, MagicMock(), prompt)()

    kernel_oqa.assert_not_called()
    dashboard.assert_called_once()
    assert prompt.metadata.openqa.kernel == []
    assert prompt.metadata.openqa.auto is dashboard.return_value.run.return_value
