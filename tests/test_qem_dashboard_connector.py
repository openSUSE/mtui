import responses

from mtui.connector.qem_dashboard import DashboardAutoOpenQA, QEMIncident
from mtui.types import RequestReviewID

API = "https://dashboard.example.com/api"


@responses.activate
def test_qem_incident_metadata():
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={
            "number": 12358,
            "packages": ["kernel-default", "kernel-ec2"],
            "channels": ["SUSE:SLE-12-SP2:Update"],
        },
        status=200,
    )

    incident = QEMIncident(RequestReviewID("SUSE:Maintenance:12358:199773"), API)

    assert incident
    assert incident.get_incident_name() == "kernel-ec2"
    assert incident.get_version() == "12-SP2"


@responses.activate
def test_qem_incident_uses_review_id_for_slfo_1_2(mock_config):
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={"number": 12358, "packages": ["bash"], "channels": []},
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/incident_settings/12358",
        json=[],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/update_settings/12358",
        json=[],
        status=200,
    )

    rrid = RequestReviewID("SUSE:SLFO:1.2:12358")
    incident = QEMIncident(rrid, API)
    DashboardAutoOpenQA(mock_config, "https://openqa.example.com", incident, rrid).run()

    assert incident.incident_number == 12358
    assert [call.request.url for call in responses.calls] == [
        f"{API}/incidents/12358",
        f"{API}/incident_settings/12358",
        f"{API}/update_settings/12358",
    ]


@responses.activate
def test_dashboard_auto_openqa_loads_incident_and_aggregate_jobs(mock_config):
    rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={"number": 12358, "packages": ["bash"], "channels": []},
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/incident_settings/12358",
        json=[
            {
                "id": 7,
                "incident": 12358,
                "version": "15-SP5",
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "settings": {"DISTRI": "sle"},
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/jobs/incident/7",
        json=[
            {
                "job_id": 1001,
                "incident_settings": 7,
                "update_settings": None,
                "name": "qam-incidentinstall",
                "job_group": "Maintenance",
                "group_id": 1,
                "status": "passed",
                "distri": "sle",
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "version": "15-SP5",
                "build": ":12358:bash",
                "obsolete": False,
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/update_settings/12358",
        json=[
            {
                "id": 23,
                "incidents": [12358],
                "product": "SLES-15-SP5",
                "arch": "x86_64",
                "build": "20240101-1",
                "repohash": "abc123",
                "settings": {"DISTRI": "sle", "VERSION": "15-SP5"},
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/jobs/update/23",
        json=[
            {
                "job_id": 1002,
                "incident_settings": None,
                "update_settings": 23,
                "name": "mau-webserver@64bit",
                "job_group": "Aggregate",
                "group_id": 2,
                "status": "failed",
                "distri": "sle",
                "flavor": "Server-DVD-Updates",
                "arch": "x86_64",
                "version": "15-SP5",
                "build": "20240101-1",
                "obsolete": False,
            }
        ],
        status=200,
    )

    incident = QEMIncident(rrid, API)
    dashboard = DashboardAutoOpenQA(
        mock_config, "https://openqa.example.com", incident, rrid
    ).run()

    assert len(dashboard.jobs) == 2
    assert any("Incident jobs" in line for line in dashboard.pp)
    assert any("Aggregate jobs" in line for line in dashboard.pp)
    assert any("mau-webserver" in line for line in dashboard.pp)
    assert dashboard.results is not None
    assert len(dashboard.results) == 1
    assert dashboard.results[0].url == (
        "https://openqa.example.com/tests/1001/file/install-logs.tar"
    )
