from mtui.types import obs

from random import randint
import pytest


@pytest.fixture(scope="function")
def r_review_id():
    return randint(0, 9999)


@pytest.fixture(scope="function")
def m_review_id():
    return randint(0, 9999999)


@pytest.mark.parametrize(
    "rrid",
    [
        "SUSE:Maintenance:{0}:{1}",
        "S:M:{0}:{1}",
        "SUSE:M:{0}:{1}",
        "S:Maintenance:{0}:{1}",
        "openSUSE:Maintenance:{0}:{1}",
    ],
)
def test_RRID_ok(r_review_id, m_review_id, rrid):
    """
    Test correct RRID is parsed successfully
    """
    rid = r_review_id
    mid = m_review_id
    rrid = obs.RequestReviewID(rrid.format(mid, rid))

    assert rrid.review_id == rid
    assert rrid.maintenance_id == mid


@pytest.mark.parametrize(
    "missing", ["SUSE:Maintenance:1", "SUSE:Maintenance:", "SUSE:Maintenance", "SUSE:"]
)
def test_parse_rrid_mc(missing):
    """
    Test parse failure: missing component
    """
    with pytest.raises(obs.MissingComponent):
        obs.RequestReviewID(missing)


@pytest.mark.parametrize(
    "cpe",
    [
        "SUSE:Maintenance:1:aa",
        "SUSE:Maintenance:aa:11",
        "d131dd02c5e6eec4",
        "DOOH:Maintenance:1:2",
        "openSUSE:boo:1:2",
    ],
)
def test_parse_rrid_cpe(cpe):
    """
    Test parse failure: componet parse errors
    """
    with pytest.raises(obs.ComponentParseError):
        obs.RequestReviewID(cpe)


def test_parse_rrid_long():
    with pytest.raises(obs.TooManyComponentsError):
        obs.RequestReviewID("SUSE:Maintenance:1:2:3")


def test_str():
    rrid = "SUSE:Maintenance:1:2"
    assert rrid == str(obs.RequestReviewID(rrid))


def test_cmp():
    rrid_1 = "SUSE:Maintenance:1:1"
    rrid_2 = "SUSE:Maintenance:1:2"

    assert obs.RequestReviewID(rrid_1) == obs.RequestReviewID(rrid_1)
    assert obs.RequestReviewID(rrid_1) != obs.RequestReviewID(rrid_2)
