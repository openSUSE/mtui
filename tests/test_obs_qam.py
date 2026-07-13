"""Tests for the native QAM operations (mtui.data_sources.obs.qam)."""

from pathlib import Path
from types import SimpleNamespace
from urllib.parse import parse_qs, urlparse

import pytest
import responses

from mtui.data_sources.obs import qam
from mtui.data_sources.obs.client import ObsClient
from mtui.data_sources.obs.errors import ObsError
from mtui.data_sources.obs.oscrc import ObsCredentials
from mtui.types.rrid import RequestReviewID

API = "https://api.suse.de"
REPORTS = "https://qam.suse.de/testreports"
FANCY = "https://qam.suse.de/reports"
RRID = RequestReviewID("SUSE:Maintenance:1:56789")
PI_RRID = RequestReviewID("SUSE:SLFO:1.1:70000")  # SLFO kind -> skips preconditions
USER = "qamuser"


def _config():
    return SimpleNamespace(
        obs_api_url=API,
        obs_request_timeout=180,
        ssl_verify=True,
        reports_url=REPORTS,
        fancy_reports_url=FANCY,
    )


def _client():
    creds = ObsCredentials(
        apiurl=API, user=USER, sshkey_path=Path("/nonexistent"), source="x"
    )
    return ObsClient(_config(), creds)


def _request_xml(state="review", reviews=""):
    return (
        f"<request id='56789'><state name='{state}'/>"
        "<action type='maintenance_release'>"
        "<source project='SUSE:Maintenance:1' package='p'/></action>"
        f"{reviews}</request>"
    )


def _group_review(group, state, *events):
    hist = "".join(
        f"<history who='{w}' when='{t}'><description>{d}</description></history>"
        for w, t, d in events
    )
    return f"<review state='{state}' by_group='{group}'>{hist}</review>"


ACCEPT = "Review got accepted"
LOG_URL = f"{REPORTS}/{RRID}/log"


def _last_query(index=-1):
    return parse_qs(urlparse(str(responses.calls[index].request.url)).query)


def _body(index=-1):
    body = responses.calls[index].request.body
    return body.decode() if isinstance(body, bytes) else str(body)


# --------------------------------------------------------------------------- #
# comment                                                                      #
# --------------------------------------------------------------------------- #
@responses.activate
def test_comment_posts_raw_unprefixed():
    responses.add(responses.POST, f"{API}/comments/request/56789", body="<ok/>")
    qam.comment(_client(), RRID, "looks good")
    assert responses.calls[0].request.body == b"looks good"


def test_comment_empty_refused():
    with pytest.raises(ObsError, match="empty comment"):
        qam.comment(_client(), RRID, "   ")


# --------------------------------------------------------------------------- #
# assign                                                                       #
# --------------------------------------------------------------------------- #
@responses.activate
def test_assign_explicit_group():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    responses.add(
        responses.GET,
        f"{API}/request",
        body="<collection/>",
    )  # previous-reject collection
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])
    q = _last_query()
    assert q["cmd"] == ["assignreview"]
    assert q["reviewer"] == [USER]
    assert q["by_group"] == ["qam-sle"]


@responses.activate
def test_assign_auto_infers_single_group():
    reviews = "<review state='new' by_group='qam-sle'/><review state='new' by_group='qam-cloud'/>"
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(reviews=reviews)
    )
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    responses.add(
        responses.GET,
        f"{API}/group",
        body='<directory><entry name="qam-sle"/></directory>',
    )
    responses.add(responses.GET, f"{API}/request", body="<collection/>")
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.assign(_client(), _config(), RRID, USER, [])
    assert _last_query()["by_group"] == ["qam-sle"]


@responses.activate
def test_assign_auto_infer_ambiguous_refused():
    reviews = "<review state='new' by_group='qam-sle'/><review state='new' by_group='qam-cloud'/>"
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(reviews=reviews)
    )
    responses.add(
        responses.GET,
        f"{API}/group",
        body='<directory><entry name="qam-sle"/><entry name="qam-cloud"/></directory>',
    )
    with pytest.raises(ObsError, match="auto-infer a single"):
        qam.assign(_client(), _config(), RRID, USER, [])


@responses.activate
def test_assign_refused_when_not_open():
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(state="accepted")
    )
    with pytest.raises(ObsError, match="not open for review"):
        qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])


@responses.activate
def test_assign_refused_when_no_testreport():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, status=404)
    with pytest.raises(ObsError, match="no testreport"):
        qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])


@responses.activate
def test_assign_previous_reject_refused_and_proceeds():
    # A related declined qam request without this user -> refuse.
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    declined = (
        "<collection><request id='9'><state name='declined'/>"
        "<review state='declined' by_group='qam-sle'/>"
        "<review state='declined' by_user='someone-else'/></request></collection>"
    )
    responses.add(responses.GET, f"{API}/request", body=declined)
    with pytest.raises(ObsError, match="previously declined"):
        qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])


@responses.activate
def test_assign_previous_reject_proceeds_when_user_was_prior_reviewer():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    declined = (
        "<collection><request id='9'><state name='declined'/>"
        "<review state='declined' by_group='qam-sle'/>"
        f"<review state='declined' by_user='{USER}'/></request></collection>"
    )
    responses.add(responses.GET, f"{API}/request", body=declined)
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])  # no raise


@responses.activate
def test_assign_previous_reject_proceeds_when_none_declined():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    open_related = (
        "<collection><request id='9'><state name='review'/>"
        "<review state='new' by_group='qam-sle'/></request></collection>"
    )
    responses.add(responses.GET, f"{API}/request", body=open_related)
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.assign(_client(), _config(), RRID, USER, ["qam-sle"])  # no raise


@responses.activate
def test_assign_skips_preconditions_for_pi():
    responses.add(responses.GET, f"{API}/request/70000", body=_request_xml())
    responses.add(responses.POST, f"{API}/request/70000", body="<ok/>")
    qam.assign(_client(), _config(), PI_RRID, USER, ["qam-sle"])
    # Only the request GET and the assign POST — no testreport / collection.
    assert len(responses.calls) == 2


# --------------------------------------------------------------------------- #
# unassign                                                                     #
# --------------------------------------------------------------------------- #
@responses.activate
def test_unassign_reverts_inferred_group():
    reviews = _group_review(
        "qam-sle", "accepted", (USER, "2020-01-01T00:00:00", ACCEPT)
    )
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(reviews=reviews)
    )
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.unassign(_client(), _config(), RRID, USER, [])
    q = _last_query()
    assert q["revert"] == ["1"]
    assert q["by_group"] == ["qam-sle"]


@responses.activate
def test_unassign_refused_without_assignment():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    with pytest.raises(ObsError, match="holds no review assignment"):
        qam.unassign(_client(), _config(), RRID, USER, ["qam-sle"])


# --------------------------------------------------------------------------- #
# approve                                                                      #
# --------------------------------------------------------------------------- #
@responses.activate
def test_approve_user_path_prefixed():
    reviews = _group_review(
        "qam-sle", "accepted", (USER, "2020-01-01T00:00:00", ACCEPT)
    )
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(reviews=reviews)
    )
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.approve(_client(), _config(), RRID, USER, [])
    q = _last_query()
    assert q["newstate"] == ["accepted"]
    assert q["by_user"] == [USER]
    assert _body().startswith("[oscqam] ")


@responses.activate
def test_approve_group_refused():
    with pytest.raises(ObsError, match="group approval is not supported"):
        qam.approve(_client(), _config(), RRID, USER, ["qam-sle"])


@responses.activate
def test_approve_refused_when_not_assigned():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    with pytest.raises(ObsError, match="not assigned"):
        qam.approve(_client(), _config(), RRID, USER, [])


@responses.activate
def test_approve_refused_when_not_passed():
    reviews = _group_review(
        "qam-sle", "accepted", (USER, "2020-01-01T00:00:00", ACCEPT)
    )
    responses.add(
        responses.GET, f"{API}/request/56789", body=_request_xml(reviews=reviews)
    )
    responses.add(responses.GET, LOG_URL, body="SUMMARY: FAILED\n")
    with pytest.raises(ObsError, match="not PASSED"):
        qam.approve(_client(), _config(), RRID, USER, [])


# --------------------------------------------------------------------------- #
# reject                                                                       #
# --------------------------------------------------------------------------- #
@responses.activate
def test_reject_writes_reason_and_declines():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: FAILED\ncomment: broken\n")
    attr = f"{API}/source/SUSE:Maintenance:1/_attribute/MAINT:RejectReason"
    responses.add(responses.GET, attr, body="<attributes/>")
    responses.add(responses.POST, attr, body="<ok/>")
    responses.add(responses.POST, f"{API}/request/56789", body="<ok/>")
    qam.reject(_client(), _config(), RRID, USER, [], "not_fixed", "some message")
    # attribute POST carries the appended reqid:flag value
    attr_post = next(
        c
        for c in responses.calls
        if c.request.url == attr and c.request.method == "POST"
    )
    assert "56789:not_fixed" in str(attr_post.request.body)
    q = _last_query()
    assert q["newstate"] == ["declined"]
    assert _body().startswith("[oscqam] ")
    # parity: the -M message is not in the decline comment
    assert "some message" not in _body()


@responses.activate
def test_reject_refused_when_not_failed():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: PASSED\n")
    with pytest.raises(ObsError, match="not FAILED"):
        qam.reject(_client(), _config(), RRID, USER, [], "not_fixed", "")


@responses.activate
def test_reject_refused_without_comment():
    responses.add(responses.GET, f"{API}/request/56789", body=_request_xml())
    responses.add(responses.GET, LOG_URL, body="SUMMARY: FAILED\n")
    with pytest.raises(ObsError, match="no comment"):
        qam.reject(_client(), _config(), RRID, USER, [], "not_fixed", "")


@responses.activate
def test_reject_pi_skips_attribute_and_summary():
    responses.add(responses.GET, f"{API}/request/70000", body=_request_xml())
    responses.add(responses.POST, f"{API}/request/70000", body="<ok/>")
    qam.reject(_client(), _config(), PI_RRID, USER, [], "not_fixed", "")
    # Only request GET + decline POST; no testreport, no attribute calls.
    assert len(responses.calls) == 2


@responses.activate
def test_reject_ignores_group_with_log(caplog):
    responses.add(responses.GET, f"{API}/request/70000", body=_request_xml())
    responses.add(responses.POST, f"{API}/request/70000", body="<ok/>")
    with caplog.at_level("INFO"):
        qam.reject(_client(), _config(), PI_RRID, USER, ["qam-sle"], "not_fixed", "")
    assert any("ignores -g" in r.message for r in caplog.records)


# --------------------------------------------------------------------------- #
# preconditions error paths                                                    #
# --------------------------------------------------------------------------- #
@responses.activate
def test_fetch_testreport_log_none_on_server_error():
    from mtui.data_sources.obs import preconditions

    responses.add(responses.GET, LOG_URL, status=500)
    assert preconditions.fetch_testreport_log(_config(), RRID) is None


@responses.activate
def test_fetch_testreport_log_none_on_connection_error():
    import requests

    from mtui.data_sources.obs import preconditions

    responses.add(responses.GET, LOG_URL, body=requests.exceptions.ConnectionError("x"))
    assert preconditions.fetch_testreport_log(_config(), RRID) is None
