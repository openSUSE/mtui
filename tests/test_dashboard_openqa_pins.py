"""Mutation-killing pinning tests for ``DashboardAutoOpenQA``.

A full mutmut run left survivors in code the suite executes but never
asserts on. These tests pin the exact current behavior:

- ``_normalize_job``: full output-dict equality for both sources,
  including the job-level vs. setting-level precedence for
  DISTRI/FLAVOR/ARCH/VERSION/BUILD and the aggregate-only
  product/repohash/incidents keys.
- ``_get_logs_url``: every ``URLs`` field (distri/arch/version/url/
  result) and the config-backed DISTRI fallback.
- ``_pretty_print_section``: the ``other`` counter increments, the
  singular ``(1 arch)`` fold suffix, and the per-group failed-jobs loop
  continuing past groups without concrete failures.
- ``_load_jobs``: settings without an ``id`` are skipped without
  aborting the rest.
- ``_job_url``: trailing-slash host normalization.
"""

from __future__ import annotations

from mtui.data_sources.qem_dashboard import DashboardAutoOpenQA, QEMIncident
from mtui.types import RequestReviewID
from mtui.types.urls import URLs

OPENQA_HOST = "https://openqa.example.com"


def _make_dashboard(mock_config) -> DashboardAutoOpenQA:
    """Build a DashboardAutoOpenQA without hitting the API."""
    rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    incident = QEMIncident.__new__(QEMIncident)
    incident.rrid = rrid
    incident.incident_number = 12358
    incident.client = None  # type: ignore[assignment]
    incident.data = {"number": 12358, "packages": ["bash"]}
    return DashboardAutoOpenQA(mock_config, OPENQA_HOST, incident, rrid)


def _incident_job(
    job_id: int,
    name: str,
    result: str,
    *,
    version: str = "15-SP5",
    flavor: str = "Server-DVD-Incidents",
    arch: str = "x86_64",
) -> dict:
    return {
        "id": job_id,
        "test": name,
        "result": result,
        "source": "incident",
        "settings": {
            "DISTRI": "sle",
            "FLAVOR": flavor,
            "ARCH": arch,
            "VERSION": version,
            "BUILD": ":12358:bash",
        },
    }


# --- _normalize_job -----------------------------------------------------------


def test_normalize_job_incident_prefers_job_level_values():
    """Job-level distri/flavor/arch/version/build win over the setting."""
    setting = {
        "id": 7,
        "flavor": "Flavor-setting",
        "arch": "arch-setting",
        "version": "ver-setting",
        "build": "build-setting",
        "settings": {"DISTRI": "distri-setting"},
    }
    job = {
        "job_id": 101,
        "name": "qam-incidentinstall",
        "status": "passed",
        "job_group": "Maintenance: Single Incidents",
        "group_id": 42,
        "obsolete": True,
        "distri": "distri-job",
        "flavor": "Flavor-job",
        "arch": "arch-job",
        "version": "ver-job",
        "build": "build-job",
    }

    normalized = DashboardAutoOpenQA._normalize_job(job, "incident", setting)

    assert normalized == {
        "id": 101,
        "test": "qam-incidentinstall",
        "result": "passed",
        "source": "incident",
        "job_group": "Maintenance: Single Incidents",
        "group_id": 42,
        "obsolete": True,
        "settings": {
            "DISTRI": "distri-job",
            "FLAVOR": "Flavor-job",
            "ARCH": "arch-job",
            "VERSION": "ver-job",
            "BUILD": "build-job",
        },
        "dashboard_setting": setting,
    }
    # The setting rides along by identity, not by copy.
    assert normalized["dashboard_setting"] is setting


def test_normalize_job_incident_falls_back_to_setting_values():
    """Missing job-level values fall back to the setting: DISTRI from the
    nested settings dict, FLAVOR/ARCH/VERSION/BUILD from the setting itself."""
    setting = {
        "id": 7,
        "flavor": "Flavor-setting",
        "arch": "arch-setting",
        "version": "ver-setting",
        "build": "build-setting",
        "settings": {"DISTRI": "distri-setting"},
    }
    job = {"job_id": 102, "name": "qam-x", "status": "failed"}

    normalized = DashboardAutoOpenQA._normalize_job(job, "incident", setting)

    assert normalized == {
        "id": 102,
        "test": "qam-x",
        "result": "failed",
        "source": "incident",
        "job_group": None,
        "group_id": None,
        "obsolete": False,
        "settings": {
            "DISTRI": "distri-setting",
            "FLAVOR": "Flavor-setting",
            "ARCH": "arch-setting",
            "VERSION": "ver-setting",
            "BUILD": "build-setting",
        },
        "dashboard_setting": setting,
    }
    # Not an aggregate job: no aggregate-only keys sneak in.
    assert "product" not in normalized
    assert "repohash" not in normalized
    assert "incidents" not in normalized


def test_normalize_job_aggregate_adds_product_repohash_incidents():
    setting = {
        "id": 23,
        "product": "SLES-15-SP5",
        "repohash": "abc123",
        "incidents": [12358, 999],
        "flavor": "Server-DVD-Updates",
        "arch": "x86_64",
        "version": "15-SP5",
        "build": "20240101-1",
        "settings": {"DISTRI": "sle"},
    }
    job = {"job_id": 201, "name": "mau-webserver", "status": "softfailed"}

    normalized = DashboardAutoOpenQA._normalize_job(job, "aggregate", setting)

    assert normalized == {
        "id": 201,
        "test": "mau-webserver",
        "result": "softfailed",
        "source": "aggregate",
        "job_group": None,
        "group_id": None,
        "obsolete": False,
        "settings": {
            "DISTRI": "sle",
            "FLAVOR": "Server-DVD-Updates",
            "ARCH": "x86_64",
            "VERSION": "15-SP5",
            "BUILD": "20240101-1",
        },
        "dashboard_setting": setting,
        "product": "SLES-15-SP5",
        "repohash": "abc123",
        "incidents": [12358, 999],
    }


def test_normalize_job_aggregate_none_incidents_becomes_empty_list():
    """`incidents: null` from the dashboard normalizes to [] (not None)."""
    setting = {"id": 23, "product": "P", "repohash": "r", "incidents": None}
    job = {"job_id": 202, "name": "mau", "status": "passed"}

    normalized = DashboardAutoOpenQA._normalize_job(job, "aggregate", setting)

    assert normalized["incidents"] == []
    assert normalized["product"] == "P"
    assert normalized["repohash"] == "r"


def test_normalize_job_tolerates_null_setting_settings():
    """`settings: null` on the setting behaves like a missing dict."""
    setting = {"id": 7, "settings": None, "flavor": "F"}
    job = {"job_id": 103, "name": "qam-y", "status": "passed"}

    normalized = DashboardAutoOpenQA._normalize_job(job, "incident", setting)

    assert normalized["settings"]["DISTRI"] is None
    assert normalized["settings"]["FLAVOR"] == "F"


# --- _get_logs_url --------------------------------------------------------------


def test_get_logs_url_pins_every_urls_field(mock_config):
    """distri/arch/version/url/result all map from the job settings, with
    the configured install distri (and '') as fallbacks for missing keys."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        {
            "test": "qam-incidentinstall",
            "result": "passed",
            "id": 1001,
            "settings": {"DISTRI": "opensuse", "ARCH": "aarch64", "VERSION": "15-SP6"},
        },
        {
            # Missing DISTRI/ARCH/VERSION keys: config default + '' fallbacks.
            "test": "qam-incidentinstall",
            "result": "softfailed",
            "id": 1002,
            "settings": {},
        },
        {
            # Not an install job: filtered out.
            "test": "qam-regression",
            "result": "passed",
            "id": 1003,
            "settings": {},
        },
        {
            # Failed install job: filtered out.
            "test": "qam-incidentinstall",
            "result": "failed",
            "id": 1004,
            "settings": {},
        },
    ]

    urls = dashboard._get_logs_url(jobs)

    assert urls == [
        URLs(
            "opensuse",
            "aarch64",
            "15-SP6",
            f"{OPENQA_HOST}/tests/1001/file/install-logs.tar",
            "passed",
        ),
        URLs(
            "sle",
            "",
            "",
            f"{OPENQA_HOST}/tests/1002/file/install-logs.tar",
            "softfailed",
        ),
    ]


def test_get_logs_url_empty_jobs_returns_none(mock_config):
    dashboard = _make_dashboard(mock_config)
    assert dashboard._get_logs_url([]) is None
    assert dashboard._get_logs_url(None) is None


# --- _pretty_print_section -------------------------------------------------------


def test_pretty_print_counts_multiple_other_jobs(mock_config):
    """Two same-group 'other'-bucket jobs count as 2, not 1."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        _incident_job(4000, "qam-parallel-a", "parallel_failed"),
        _incident_job(4001, "qam-parallel-b", "parallel_failed"),
        _incident_job(4002, "qam-pass", "passed"),
    ]

    out = "".join(dashboard._pretty_print(jobs))

    assert "other: 2" in out
    assert "total: 3" in out


def test_pretty_print_single_arch_fold_uses_singular_arch(mock_config):
    """A one-arch folded group renders '(1 arch)', not '(1 arches)'."""
    dashboard = _make_dashboard(mock_config)
    jobs = [_incident_job(5000, "qam-ok", "passed")]

    out = "".join(dashboard._pretty_print(jobs))

    assert "(1 arch)" in out
    assert "(1 arches)" not in out


def test_pretty_print_failed_jobs_loop_skips_group_without_concrete_failures(
    mock_config,
):
    """A problem group whose issues live only in 'other' is skipped in the
    Failed jobs listing, but later groups with real failures still render."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        # First problem group: only a parallel_failed job (no failed_by_group
        # entry) -- the failed-jobs loop must continue past it.
        _incident_job(6000, "qam-parallel", "parallel_failed", flavor="Flavor-A"),
        # Second problem group: a concrete failure that must still be listed.
        _incident_job(6001, "qam-dead", "failed", flavor="Flavor-B"),
    ]

    out = "".join(dashboard._pretty_print(jobs))

    assert "Failed jobs:" in out
    assert "15-SP5 / Flavor-B / x86_64 (1 failed):" in out
    assert "qam-dead" in out
    # The other-only group still shows up in the Summary as a problem group.
    assert "flavor: Flavor-A" in out
    assert "other: 1" in out


# --- _load_jobs: malformed settings are skipped, not fatal -------------------------


def test_load_jobs_skips_settings_without_id(mock_config):
    """A settings entry lacking 'id' is dropped; later entries still load."""
    dashboard = _make_dashboard(mock_config)

    class _FakeClient:
        @staticmethod
        def incident_settings(_n):
            return [{"settings": {}}, {"id": 11, "settings": {}}]

        @staticmethod
        def update_settings(_n):
            return [{"settings": {}}, {"id": 21, "settings": {}}]

        @staticmethod
        def incident_jobs(sid):
            return [{"job_id": 1000 + sid, "name": f"qam-{sid}", "status": "passed"}]

        @staticmethod
        def update_jobs(sid):
            return [{"job_id": 2000 + sid, "name": f"mau-{sid}", "status": "passed"}]

    dashboard.client = _FakeClient()  # type: ignore[assignment]  # ty: ignore[invalid-assignment]

    jobs = dashboard._load_jobs()

    assert [job["test"] for job in jobs] == ["qam-11", "mau-21"]


# --- _job_url ----------------------------------------------------------------------


def test_job_url_strips_trailing_slash_from_host():
    assert (
        DashboardAutoOpenQA._job_url("https://openqa.example.com/", 42)
        == "https://openqa.example.com/tests/42"
    )
    assert (
        DashboardAutoOpenQA._job_url("https://openqa.example.com", 42)
        == "https://openqa.example.com/tests/42"
    )


def test_job_url_none_job_id_is_empty():
    assert DashboardAutoOpenQA._job_url(OPENQA_HOST, None) == ""
