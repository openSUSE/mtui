"""Tests for the ``openqa_jobs`` command's pending-job handling.

Regression tests: a job that has not finished yet (openQA keeps its
``result`` at ``none`` until the job is done) was counted as a failure
by ``--failed`` and painted red in the listing, making an in-progress
build look like it had a dozen failures.
"""

from argparse import Namespace
from io import StringIO
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

from mtui.commands.openqa_jobs import OpenQAJobs, _display_result
from mtui.data_sources.oqa_search import JobResult
from mtui.types import RequestReviewID


def _job(result: str, state: str = "done", test: str = "t", arch: str = "x86_64"):
    return JobResult(
        job_id=1, test=test, arch=arch, result=result, url="u", state=state
    )


def _run(jobs, **arg_overrides) -> str:
    """Run OpenQAJobs against canned jobs and return its stdout.

    The colour helpers are patched to visible tags so the tests can
    assert the colour *classification* without depending on the runtime
    colour mode.
    """
    prompt = MagicMock()
    prompt.metadata.__bool__ = lambda self: True
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:1:1")
    out = StringIO()
    args = Namespace(
        all=False,
        failed=False,
        arch=None,
        url_openqa=None,
        url_dashboard_qam=None,
    )
    for key, value in arg_overrides.items():
        setattr(args, key, value)
    config = MagicMock()
    config.openqa_instance = "https://openqa.example.com"
    config.qem_dashboard_api = "http://dashboard.example.com/api"
    with (
        patch("mtui.commands.openqa_jobs.oqa.set_verify"),
        patch(
            "mtui.commands.openqa_jobs.oqa.get_incident_info",
            return_value=("build7", 0),
        ),
        patch("mtui.commands.openqa_jobs.oqa.incident_jobs", return_value=jobs),
        patch("mtui.commands.openqa_jobs.green", lambda s: f"<G>{s}</G>"),
        patch("mtui.commands.openqa_jobs.yellow", lambda s: f"<Y>{s}</Y>"),
        patch("mtui.commands.openqa_jobs.red", lambda s: f"<R>{s}</R>"),
    ):
        OpenQAJobs(args, config, SimpleNamespace(stdout=out), prompt)()
    return out.getvalue()


def test_failed_excludes_pending_jobs():
    """--failed must not list scheduled/running jobs as failures."""
    output = _run(
        [
            _job("failed", test="real_failure"),
            _job("none", state="running", test="still_running"),
            _job("none", state="scheduled", test="not_started"),
            _job("passed", test="ok"),
        ],
        failed=True,
    )

    assert "real_failure" in output
    assert "still_running" not in output
    assert "not_started" not in output
    assert "ok" not in output
    # The count summary reflects only the one genuine failure.
    assert "(1): failed=1" in output


def test_listing_shows_pending_state_in_yellow_not_a_red_none():
    """An unfinished job shows its state, coloured like the neutral ones."""
    output = _run(
        [
            _job("none", state="running", test="still_running"),
            _job("none", state="scheduled", test="not_started"),
            _job("passed", test="ok"),
        ]
    )

    assert "<Y>running" in output
    assert "<Y>scheduled" in output
    assert "<G>passed" in output
    # The meaningless raw result must not surface anywhere -- neither as a
    # red row nor in the count summary.
    assert "none" not in output
    assert "passed=1, running=1, scheduled=1" in output


def test_real_failures_still_red_and_listed():
    """The fix must not soften genuine failures."""
    output = _run([_job("failed"), _job("incomplete"), _job("parallel_failed")])

    assert "<R>failed" in output
    assert "<R>incomplete" in output
    assert "<R>parallel_failed" in output


def test_display_result_falls_back_to_pending_without_state():
    """Old/degraded API data: no state either -- label it pending."""
    assert _display_result(_job("", state="")) == "pending"
    assert _display_result(_job("none", state="")) == "pending"
    assert _display_result(_job("softfailed", state="done")) == "softfailed"


def test_failed_on_all_pending_build_names_the_pending_work():
    """--failed on an in-progress build must not claim there are no jobs.

    With every job pending the filter empties the list; the old generic
    "No openQA jobs" message made an in-progress build indistinguishable
    from one with no jobs at all.
    """
    output = _run(
        [
            _job("none", state="running"),
            _job("none", state="scheduled"),
            _job("passed"),
        ],
        failed=True,
    )

    assert "No failed openQA jobs" in output
    assert "2 of 3 still pending" in output
    assert "No openQA jobs" not in output


def test_failed_on_all_passed_build_reports_no_failed_jobs():
    """--failed with only finished passing jobs: accurate, no pending note."""
    output = _run([_job("passed"), _job("softfailed")], failed=True)

    assert "No failed openQA jobs" in output
    assert "pending" not in output


def test_no_jobs_at_all_keeps_the_original_message():
    """A genuinely empty build still reports no jobs."""
    output = _run([], failed=True)

    assert "No openQA jobs for build" in output
