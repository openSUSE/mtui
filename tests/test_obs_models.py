"""Tests for the OBS XML parsers (mtui.data_sources.obs.models)."""

import pytest

from mtui.data_sources.obs import models
from mtui.data_sources.obs.errors import ObsError

REQUEST_XML = """
<request id="42">
  <action type="maintenance_release">
    <source project="SUSE:Maintenance:130" package="pkg.SUSE_SLE-12_Update"/>
    <target project="SUSE:SLE-12:Update" package="pkg.130"/>
  </action>
  <review state="review" when="2014-11-14T11:12:53" who="anon" by_user="anon"/>
  <review state="accepted" by_group="qam-sle">
    <history who="alice" when="2017-09-06T08:06:39">
      <description>Review got accepted</description>
      <comment>review for group qam-sle</comment>
    </history>
  </review>
  <review state="new" by_group="qam-cloud"/>
  <state name="review" who="anon" when="2014-12-01T14:46:23"/>
</request>
"""


def test_parse_request_core_fields():
    req = models.parse_request(REQUEST_XML)
    assert req.reqid == "42"
    assert req.state == "review"
    assert req.src_project == "SUSE:Maintenance:130"
    assert len(req.reviews) == 3


def test_parse_request_reviews_and_nested_history():
    req = models.parse_request(REQUEST_XML)
    sle = next(r for r in req.reviews if r.by_group == "qam-sle")
    assert sle.state == "accepted"
    assert len(sle.history) == 1
    assert sle.history[0].who == "alice"
    assert sle.history[0].when == "2017-09-06T08:06:39"
    assert sle.history[0].description == "Review got accepted"
    user_review = next(r for r in req.reviews if r.by_user == "anon")
    assert user_review.by_group is None


def test_parse_request_missing_source_and_state():
    req = models.parse_request('<request id="9"></request>')
    assert req.src_project is None
    assert req.state == ""
    assert req.reviews == ()


def test_parse_group_directory():
    xml = '<directory count="2"><entry name="qam-sle"/><entry name="qam-cloud"/></directory>'
    assert models.parse_group_directory(xml) == ["qam-sle", "qam-cloud"]


def test_parse_group_directory_empty():
    assert models.parse_group_directory('<directory count="0"/>') == []


def test_parse_reject_reason_values():
    xml = (
        '<attributes><attribute namespace="MAINT" name="RejectReason">'
        "<value>100:not_fixed</value><value> 101:regression </value>"
        "</attribute></attributes>"
    )
    assert models.parse_reject_reason_values(xml) == ["100:not_fixed", "101:regression"]


def test_parse_reject_reason_empty_attributes():
    assert models.parse_reject_reason_values("<attributes/>") == []


def test_build_reject_reason_body_roundtrips():
    body = models.build_reject_reason_body(["100:not_fixed", "200:regression"])
    assert 'namespace="MAINT"' in body
    assert 'name="RejectReason"' in body
    assert models.parse_reject_reason_values(body) == [
        "100:not_fixed",
        "200:regression",
    ]


def test_is_qam_group():
    assert models.is_qam_group("qam-sle")
    assert not models.is_qam_group("qam-auto")
    assert not models.is_qam_group("qam-openqa")
    assert not models.is_qam_group("legal-auto")


@pytest.mark.parametrize(
    "parser",
    [
        models.parse_request,
        models.parse_request_collection,
        models.parse_group_directory,
        models.parse_reject_reason_values,
    ],
)
def test_parsers_refuse_dtd(parser):
    """A DTD (billion-laughs vector) is refused before parsing."""
    billion_laughs = (
        '<?xml version="1.0"?>'
        '<!DOCTYPE lolz [<!ENTITY lol "lol">'
        '<!ENTITY lol2 "&lol;&lol;&lol;">]>'
        '<request><state name="&lol2;"/></request>'
    )
    with pytest.raises(ObsError, match="DTD"):
        parser(billion_laughs)
