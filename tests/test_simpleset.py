from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.simpleset import SetLocation, SetWorkflow
from mtui.types import OpenQAResults, RequestReviewID


class _FakeConfig:
    """Minimal config whose location setter mimics Config's accept/reject."""

    def __init__(self, accept: bool) -> None:
        self._loc = "foo"
        self._accept = accept

    @property
    def location(self) -> str:
        return self._loc

    @location.setter
    def location(self, value: str) -> None:
        # Real Config rejects an unknown location and keeps the old value.
        if self._accept:
            self._loc = value


def _run_set_location(accept: bool, site: str, initial_hosts):
    prompt = MagicMock()
    prompt.metadata.hostnames = set(initial_hosts)
    SetLocation(Namespace(site=[site]), _FakeConfig(accept), MagicMock(), prompt)()
    return prompt.metadata


def test_set_location_resets_hostnames_on_change():
    """A successful set_location drops template/old-location refhosts."""
    meta = _run_set_location(True, "bar", ["foo-host-1", "foo-host-2"])
    assert meta.hostnames == set()


def test_set_location_keeps_hostnames_when_rejected():
    """A rejected location leaves the working refhost set untouched."""
    meta = _run_set_location(False, "atlantis", ["foo-host-1"])
    assert meta.hostnames == {"foo-host-1"}


def test_set_workflow_auto_uses_rrid(mock_config):
    prompt = MagicMock()
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = OpenQAResults()
    prompt.display = MagicMock()
    prompt.targets = MagicMock()

    args = Namespace(workflow="auto")
    mock_config.auto = False
    mock_config.kernel = False

    with patch("mtui.commands.simpleset.DashboardAutoOpenQA") as dashboard:
        dashboard.return_value.run.return_value.results = []
        SetWorkflow(args, mock_config, MagicMock(), prompt)()

    dashboard.assert_called_once_with(
        mock_config,
        mock_config.openqa_instance,
        prompt.metadata.incident,
        prompt.metadata.rrid,
    )
