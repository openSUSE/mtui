from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.simpleset import SetWorkflow
from mtui.types import RequestReviewID


def test_set_workflow_auto_uses_rrid(mock_config):
    prompt = MagicMock()
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = {"auto": None, "kernel": []}
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
