from nose.tools import eq_, ok_, raises

from mtui.types.md5 import MD5Hash
from mtui.types import obs

from .utils import rand_maintenance_id
from .utils import rand_review_id

@raises(ValueError)
def test_MD5Hash_invalid():
    """Test MD5Hash() raises when given invalid hash"""
    MD5Hash("foo")

def test_MD5Hash_equal():
    eq_(MD5Hash("472ababb814d21290b4bef7bc4c815d9"),
        MD5Hash("472ababb814d21290b4bef7bc4c815d9"))

def test_MD5Hash_not_equal():
    ok_(not MD5Hash("472ababb814d21290b4bef7bc4c815d9") ==
        MD5Hash("072ababb814d21290b4bef7bc4c815d9"))

@raises(TypeError)
def test_MD5Hash_TypeError():
    MD5Hash("472ababb814d21290b4bef7bc4c815d9") == "foo"


def helper_parse_reviewid(rrid):
    return obs.RequestReviewID(rrid)

def test_RRID_ok():
    """
    Test correct RRID is parsed successfully
    """
    rid = rand_review_id()
    mid = rand_maintenance_id()
    rrid = helper_parse_reviewid("SUSE:Maintenance:{0}:{1}".format(
        mid, rid))

    eq_(rrid.review_id, rid)
    eq_(rrid.maintenance_id, mid)

@raises(obs.MissingComponent)
def test_parse_rrid_w0():
    """
    Test parse failure: missing rid
    """
    helper_parse_reviewid("SUSE:Maintenance:1:")

@raises(obs.MissingComponent)
def test_parse_rrid_w1():
    """
    Test parse failure: missing mid
    """
    helper_parse_reviewid("SUSE:Maintenance:")

@raises(obs.ComponentParseError)
def test_parse_rrid_w2():
    """
    Test parse failure: md5 sum instead
    """
    helper_parse_reviewid("a93bcc098674a50ea93791fc528bdd9f")

@raises(obs.ComponentParseError)
def test_parse_rrid_w3():
    """
    Test parse failure: invalid 3rd component value
    """
    helper_parse_reviewid("SUSE:Maintenance:Quux:Doh")


@raises(obs.TooManyComponentsError)
def test_parse_rrid_w4():
    """
    Test parse failure: too many components
    """
    helper_parse_reviewid("Foo:Bar:Quux:Doh:Kek")

@raises(obs.ComponentParseError)
def test_parse_rrid_w5():
    """
    Test parse failure: invalid 1st component value
    """
    helper_parse_reviewid("Foo:Maintenance:1:1")
