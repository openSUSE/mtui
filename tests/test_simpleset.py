from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.simpleset import SetWorkflow
from mtui.types import OpenQAResults, RequestReviewID, Workflow


def test_set_workflow_auto_uses_rrid(mock_config):
    prompt = MagicMock()
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = OpenQAResults()
    prompt.metadata.workflow = Workflow.MANUAL
    prompt.display = MagicMock()
    prompt.targets = MagicMock()

    args = Namespace(workflow="auto")

    with patch("mtui.commands.simpleset.DashboardAutoOpenQA") as dashboard:
        dashboard.return_value.run.return_value.results = []
        SetWorkflow(args, mock_config, MagicMock(), prompt)()

    dashboard.assert_called_once_with(
        mock_config,
        mock_config.openqa_instance,
        prompt.metadata.incident,
        prompt.metadata.rrid,
    )


def test_set_workflow_is_fanout():
    """set_workflow fans out across every loaded template by default."""
    assert SetWorkflow.scope == "fanout"


def test_set_workflow_accepts_template_flag():
    ns = SetWorkflow.parse_args("-T SUSE:Maintenance:1:1 manual", MagicMock())
    assert ns.workflow == "manual"
    assert ns.template == "SUSE:Maintenance:1:1"
    assert ns.all_templates is False


def test_set_workflow_accepts_all_templates_flag():
    ns = SetWorkflow.parse_args("--all-templates manual", MagicMock())
    assert ns.all_templates is True
    assert ns.template is None


def test_set_workflow_complete_offers_template_rrids():
    templates = MagicMock()
    templates.rrids.return_value = ["SUSE:Maintenance:1:1"]
    out = SetWorkflow.complete({"templates": templates}, "", "set_workflow ", 13, 13)
    assert "SUSE:Maintenance:1:1" in out
