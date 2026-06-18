"""Tests for the ``openqa_overview`` interactive command.

Mirrors the style of ``tests/test_reloadoqa.py``: unit-only, prompt and
connector helpers patched, asserts on the side effects (stdout via
``println``, structured payload stored on
``prompt.metadata.openqa.overview``).
"""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.openqa_overview import OpenQAOverview
from mtui.data_sources.oqa_search import (
    BuildCheckResult,
    GroupResult,
    VersionResult,
)
from mtui.types import OpenQAResults, RequestReviewID


def _build_prompt() -> MagicMock:
    """Build a prompt mock with truthy metadata and an empty OpenQAResults."""
    prompt = MagicMock()
    prompt.metadata.__bool__ = lambda self: True
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = OpenQAResults()
    prompt.display = MagicMock()
    prompt.targets = MagicMock()
    return prompt


def _args(**overrides) -> Namespace:
    """A Namespace pre-populated with the command's defaults."""
    defaults: dict[str, object] = {
        "no_aggregated": False,
        "days": 5,
        "aggregated_groups": ["core"],
        "url_openqa": None,
        "url_dashboard_qam": None,
        "url_qam": None,
        "test_pattern": None,
        "export": False,
        "no_fetch": False,
    }
    defaults.update(overrides)
    return Namespace(**defaults)


def test_openqa_overview_runs_all_three_sections_and_stores_payload(mock_config):
    """Happy path: get_incident_info returns versions; all three sections run."""
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    single_rows = [
        VersionResult(version="15-SP5", url="u1", status="passed"),
        VersionResult(version="15-SP4", url="u2", status="failed", failed_count=3),
    ]
    aggregated_rows = [
        GroupResult(
            group="core",
            versions=[VersionResult(version="15-SP5", url="u3", status="passed")],
        )
    ]
    build_check_rows = [BuildCheckResult(url="http://qam/bash.log", matches=["ok"])]

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:bash", ["15-SP4", "15-SP5"]),
        ) as gii,
        patch(
            "mtui.commands.openqa_overview.oqa.single_incidents",
            return_value=single_rows,
        ) as si,
        patch(
            "mtui.commands.openqa_overview.oqa.aggregated_updates",
            return_value=aggregated_rows,
        ) as au,
        patch(
            "mtui.commands.openqa_overview.oqa.build_checks",
            return_value=build_check_rows,
        ) as bc,
    ):
        OpenQAOverview(_args(), mock_config, MagicMock(), prompt)()

    # All four entry points called once each.
    gii.assert_called_once()
    si.assert_called_once()
    au.assert_called_once()
    bc.assert_called_once()

    # Stored payload contains the three sections.
    overview = prompt.metadata.openqa.overview
    assert overview is not None
    assert overview.single_incidents == single_rows
    assert overview.aggregated_updates == aggregated_rows
    assert overview.build_checks == build_check_rows
    assert overview.skip_aggregated is False


def test_openqa_overview_no_aggregated_flag_skips_aggregated_section(mock_config):
    """``--no-aggregated`` skips the aggregated_updates fetch entirely."""
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:bash", ["15-SP5"]),
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.single_incidents",
            return_value=[VersionResult("15-SP5", "u", "passed")],
        ),
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates") as au,
        patch("mtui.commands.openqa_overview.oqa.build_checks", return_value=[]),
    ):
        OpenQAOverview(_args(no_aggregated=True), mock_config, MagicMock(), prompt)()

    au.assert_not_called()
    assert prompt.metadata.openqa.overview.aggregated_updates == []
    assert prompt.metadata.openqa.overview.skip_aggregated is True


def test_openqa_overview_no_versions_skips_openqa_sections(mock_config):
    """When the dashboard reports no versions, openQA sections are skipped."""
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:bash", None),
        ),
        patch("mtui.commands.openqa_overview.oqa.single_incidents") as si,
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates") as au,
        patch("mtui.commands.openqa_overview.oqa.build_checks", return_value=[]) as bc,
    ):
        OpenQAOverview(_args(), mock_config, MagicMock(), prompt)()

    si.assert_not_called()
    au.assert_not_called()
    # build_checks still runs (it's independent of openQA versions).
    bc.assert_called_once()


@pytest.mark.parametrize(
    (
        "rrid",
        "expected_product",
        "expected_incident_id",
        "expected_request_id",
        "expected_url",
    ),
    [
        (
            "SUSE:SLFO:1.2:5348",
            "SLFO",
            "1.2",
            5348,
            "https://qam.example.com/testreports/SUSE:SLFO:1.2:5348/build_checks",
        ),
        (
            "SUSE:Maintenance:12345:67891",
            "Maintenance",
            12345,
            67891,
            "https://qam.example.com/testreports/"
            "SUSE:Maintenance:12345:67891/build_checks",
        ),
    ],
)
def test_openqa_overview_passes_maintenance_id_and_builds_correct_url(
    mock_config,
    rrid,
    expected_product,
    expected_incident_id,
    expected_request_id,
    expected_url,
):
    """The command passes the maintenance_id (not the request id) to
    build_checks, yielding the correct QAM URL for both SLFO and Maintenance.

    Regression: for ``SUSE:SLFO:1.2:5348`` the command previously passed the
    Dashboard ``effective_incident_id`` (the request id ``5348``) as the
    incident id, so the QAM URL became ``SUSE:SLFO:5348:5348`` (a 404)
    instead of ``SUSE:SLFO:1.2:5348``. The Maintenance case must keep working.
    """
    prompt = _build_prompt()
    prompt.metadata.id = rrid
    prompt.metadata.rrid = RequestReviewID(rrid)
    mock_config.reports_url = "https://qam.example.com/testreports"

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            # None versions -> single/aggregated sections are skipped.
            return_value=(f":{expected_request_id}:bash", None),
        ),
        patch("mtui.commands.openqa_overview.oqa.single_incidents", return_value=[]),
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates", return_value=[]),
        patch("mtui.commands.openqa_overview.oqa.build_checks", return_value=[]) as bc,
    ):
        OpenQAOverview(_args(), mock_config, MagicMock(), prompt)()

    bc.assert_called_once()

    product, incident_id, request_id, _packages, url_qam = bc.call_args.args[:5]
    assert product == expected_product
    assert incident_id == expected_incident_id
    assert request_id == expected_request_id

    base_url = (
        f"{url_qam}/testreports/SUSE:{product}:{incident_id}:{request_id}/build_checks"
    )
    assert base_url == expected_url


def test_openqa_overview_cli_overrides_take_precedence_over_config(mock_config):
    """Explicit --url-* flags override the values derived from config."""
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa-config.example.com"
    mock_config.qem_dashboard_api = "https://dashboard-config.example.com/api"
    mock_config.reports_url = "https://qam-config.example.com/testreports"

    args = _args(
        url_openqa="https://openqa-override.example.com",
        url_dashboard_qam="https://dashboard-override.example.com",
        url_qam="https://qam-override.example.com",
    )

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:bash", ["15-SP5"]),
        ) as gii,
        patch(
            "mtui.commands.openqa_overview.oqa.single_incidents",
            return_value=[],
        ) as si,
        patch(
            "mtui.commands.openqa_overview.oqa.aggregated_updates",
            return_value=[],
        ) as au,
        patch(
            "mtui.commands.openqa_overview.oqa.build_checks",
            return_value=[],
        ) as bc,
    ):
        OpenQAOverview(args, mock_config, MagicMock(), prompt)()

    # Dashboard URL is the override
    assert gii.call_args.args[0] == "https://dashboard-override.example.com"
    # openQA URL is the override
    assert si.call_args.args[2] == "https://openqa-override.example.com"
    assert au.call_args.args[4] == "https://openqa-override.example.com"
    # QAM URL is the override
    assert bc.call_args.args[4] == "https://qam-override.example.com"


def test_openqa_overview_derives_dashboard_and_qam_urls_from_config(mock_config):
    """Defaults: strip /api from qem_dashboard_api and /testreports from reports_url."""
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:bash", ["15-SP5"]),
        ) as gii,
        patch("mtui.commands.openqa_overview.oqa.single_incidents", return_value=[]),
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates", return_value=[]),
        patch("mtui.commands.openqa_overview.oqa.build_checks", return_value=[]) as bc,
    ):
        OpenQAOverview(_args(), mock_config, MagicMock(), prompt)()

    # /api stripped from dashboard URL
    assert gii.call_args.args[0] == "https://dashboard.example.com"
    # /testreports stripped from qam URL
    assert bc.call_args.args[4] == "https://qam.example.com"


def test_openqa_overview_strips_obs_timestamp_from_printed_build_checks(mock_config):
    """Printed build-check lines have the OBS `[  Ns]` prefix removed.

    The stored payload keeps the raw lines so downstream consumers still
    see the original timestamps.
    """
    prompt = _build_prompt()
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    raw_matches = [
        "[   28s] All 9 tests passed",
        "[  158s] All 9 tests passed",
    ]
    build_check_rows = [
        BuildCheckResult(url="http://qam/xz.x86_64.log", matches=list(raw_matches))
    ]
    sys_mock = MagicMock()

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:xz", ["15-SP5"]),
        ),
        patch("mtui.commands.openqa_overview.oqa.single_incidents", return_value=[]),
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates", return_value=[]),
        patch(
            "mtui.commands.openqa_overview.oqa.build_checks",
            return_value=build_check_rows,
        ),
    ):
        OpenQAOverview(_args(), mock_config, sys_mock, prompt)()

    # Stitch every chunk written to sys.stdout into one string.
    written = "".join(call.args[0] for call in sys_mock.stdout.write.call_args_list)

    # No `[  Ns]` prefixes anywhere in the printed output.
    assert "[   28s]" not in written
    assert "[  158s]" not in written
    # The actual text content survives.
    assert "All 9 tests passed" in written

    # The stored payload keeps the raw timestamps untouched.
    assert prompt.metadata.openqa.overview.build_checks[0].matches == raw_matches


def test_openqa_overview_complete_offers_all_flags():
    """Tab completion suggests the command's flags."""
    state = {"hosts": []}
    out = OpenQAOverview.complete(state, "", "openqa_overview ", 17, 17)
    # Every defined flag should be offered when nothing is typed yet.
    expected = {
        "--no-aggregated",
        "--days",
        "--aggregated-groups",
        "--url-openqa",
        "--url-dashboard-qam",
        "--url-qam",
        "--test-pattern",
        "--export",
        "--no-fetch",
    }
    assert expected.issubset(set(out))


# --- --export flag ---


def test_openqa_overview_export_writes_block_to_testreport_log(mock_config, tmp_path):
    """--export writes the rendered overview into the loaded testreport log."""
    log_path = tmp_path / "log"
    log_path.write_text(
        "Maintenance Test\n"
        "\n"
        "regression tests:\n"
        "-----------------\n"
        "\n"
        "(put your details here)\n"
        "\n"
        "build log review:\n"
        "-----------------\n"
    )

    prompt = _build_prompt()
    prompt.metadata.path = log_path
    mock_config.openqa_instance = "https://openqa.example.com"
    mock_config.qem_dashboard_api = "https://dashboard.example.com/api"
    mock_config.reports_url = "https://qam.example.com/testreports"

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:xz", ["15-SP5"]),
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.single_incidents",
            return_value=[VersionResult("15-SP5", "u1", "passed")],
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.aggregated_updates",
            return_value=[],
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.build_checks",
            return_value=[BuildCheckResult(url="https://qam/xz.log", matches=["ok"])],
        ),
    ):
        OpenQAOverview(_args(export=True), mock_config, MagicMock(), prompt)()

    written = log_path.read_text()
    # Markers and content present in the file on disk.
    assert "<!-- mtui openqa_overview begin -->" in written
    assert "<!-- mtui openqa_overview end -->" in written
    assert "## OpenQA Overview" in written
    assert "15-SP5" in written
    assert "https://qam/xz.log" in written


def test_openqa_overview_export_is_idempotent_on_repeat(mock_config, tmp_path):
    """Re-running --export replaces the prior block instead of duplicating."""
    log_path = tmp_path / "log"
    log_path.write_text(
        "regression tests:\n"
        "-----------------\n"
        "\n"
        "(put your details here)\n"
        "\n"
        "build log review:\n"
        "-----------------\n"
    )

    prompt = _build_prompt()
    prompt.metadata.path = log_path
    mock_config.openqa_instance = "https://o"
    mock_config.qem_dashboard_api = "https://d/api"
    mock_config.reports_url = "https://q/testreports"

    side_effects_single = [
        [VersionResult("15-SP5", "u1", "passed")],
        [VersionResult("12-SP5", "u2", "failed", failed_count=1)],
    ]

    with (
        patch(
            "mtui.commands.openqa_overview.oqa.get_incident_info",
            return_value=(":12358:xz", ["15-SP5"]),
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.single_incidents",
            side_effect=side_effects_single,
        ),
        patch(
            "mtui.commands.openqa_overview.oqa.aggregated_updates",
            return_value=[],
        ),
        patch("mtui.commands.openqa_overview.oqa.build_checks", return_value=[]),
    ):
        OpenQAOverview(_args(export=True), mock_config, MagicMock(), prompt)()
        # Fresh prompt for second call so cache doesn't short-circuit.
        prompt2 = _build_prompt()
        prompt2.metadata.path = log_path
        OpenQAOverview(_args(export=True), mock_config, MagicMock(), prompt2)()

    written = log_path.read_text()
    # Still exactly one block.
    assert written.count("<!-- mtui openqa_overview begin -->") == 1
    # Latest data is what landed.
    assert "12-SP5" in written
    assert "15-SP5" not in written


def test_openqa_overview_no_fetch_uses_cached_overview(mock_config, tmp_path):
    """--no-fetch reuses metadata.openqa.overview without hitting the network."""
    from mtui.types import OpenQAOverviewResult

    log_path = tmp_path / "log"
    log_path.write_text(
        "regression tests:\n-----------------\n\nbuild log review:\n-----------------\n"
    )

    prompt = _build_prompt()
    prompt.metadata.path = log_path
    prompt.metadata.openqa.overview = OpenQAOverviewResult(
        single_incidents=[VersionResult("15-SP5", "cached_url", "passed")],
    )

    with (
        patch("mtui.commands.openqa_overview.oqa.get_incident_info") as gii,
        patch("mtui.commands.openqa_overview.oqa.single_incidents") as si,
        patch("mtui.commands.openqa_overview.oqa.aggregated_updates") as au,
        patch("mtui.commands.openqa_overview.oqa.build_checks") as bc,
    ):
        OpenQAOverview(
            _args(no_fetch=True, export=True),
            mock_config,
            MagicMock(),
            prompt,
        )()

    # No network calls.
    gii.assert_not_called()
    si.assert_not_called()
    au.assert_not_called()
    bc.assert_not_called()
    # And the cached payload was exported.
    assert "cached_url" in log_path.read_text()
