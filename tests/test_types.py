from random import randint

import pytest

from mtui.exceptions import (
    ComponentParseError,
    MissingComponent,
    TooManyComponentsError,
)
from mtui.types import RequestReviewID


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
        "S:S:1.1:{0}",
        "SUSE:S:1.1:{0}",
        "SUSE:SLFO:1.1:{0}",
        "S:SLFO:1.1:{0}",
    ],
)
def test_RRID_ok(r_review_id, m_review_id, rrid):
    """
    Test correct RRID is parsed successfully
    """
    rid = r_review_id
    mid = m_review_id
    rrid = RequestReviewID(rrid.format(mid, rid))

    if rrid.kind != "SLFO":
        assert rrid.review_id == rid
    assert rrid.maintenance_id == mid


@pytest.mark.parametrize("missing", ["SUSE:Maintenance", "SUSE:M", "SUSE"])
def test_parse_rrid_mc(missing):
    """
    Test parse failure: missing component
    """
    with pytest.raises(MissingComponent):
        RequestReviewID(missing)


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
    with pytest.raises(ComponentParseError):
        RequestReviewID(cpe)


def test_parse_rrid_long():
    with pytest.raises(TooManyComponentsError):
        RequestReviewID("SUSE:Maintenance:1:2:3")


def test_str():
    rrid = "SUSE:Maintenance:1:2"
    assert rrid == str(RequestReviewID(rrid))


def test_cmp():
    rrid_1 = "SUSE:Maintenance:1:1"
    rrid_1_1 = "S:M:1:1"
    rrid_2 = "SUSE:Maintenance:1:2"

    assert RequestReviewID(rrid_1) == RequestReviewID(rrid_1_1)
    assert RequestReviewID(rrid_1) != RequestReviewID(rrid_2)
