from nose.tools import eq_, raises

from mtui.types import obs
from mtui import messages

from .utils import rand_maintenance_id
from .utils import rand_review_id
from .utils import merged_dict


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

def test_RRID_openSUSE_ok():
    """
    Test correct RRID is parsed successfully
    """
    rid = rand_review_id()
    mid = rand_maintenance_id()
    rrid = helper_parse_reviewid("openSUSE:Maintenance:{0}:{1}".format(
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

def test_parse_disturl():
    def check(attrs, url_fmt):
        url = obs.DistURL(url_fmt.format(**attrs))

        for attr, val in list(attrs.items()):
            eq_(getattr(url, attr), val)

    attrs = [
        dict(
            # standard sle 12
              project = "SUSE:SLE-12:GA"
            , commit  = "ed5e7593527addc3b4cff541962d27ea"
            , package = "rpm-python"
        ),
        dict(
            # testing repo sle 12
              project = "SUSE:Maintenance:264"
            , commit  = "9e0d11e6929909ba404a80012e885e35"
            , package = "rpm-python.SUSE_SLE-12_Update"
        ),
        dict(
            # standard sle 11
              project = "SUSE:SLE-11:GA"
            , commit  = "1967955687035ffac842159d17d2f114"
            , package = "perl-Net-DNS"
        ),
    ]

    standard_url = "obs://foo/{project}/standard/{commit}-{package}"
    urls = [
          standard_url
        , "obs://foo/{project}/SUSE_SLE-12_Update/{commit}-{package}"
        , standard_url
    ]

    for attrs, url in zip(attrs, urls):
        yield check, attrs, url

def test_parse_disturl_failure():
    valid = dict(
          project   = "SUSE:SLE-12:GA"
        , commit    = "1967955687035ffac842159d17d2f114"
        , package   = "rpm-python.SUSE_SLE-12_Update"
        , repo      = "standard"
        , protocol  = "obs"
        , hostname  = "foo"
    )
    fmt = "{protocol}://{hostname}/{project}/{repo}/{commit}-{package}"

    try:
        obs.DistURL(fmt.format(**valid))
    except messages.InvalidOBSDistURL:
        assert False, \
            "Test broken in tautological way leading to false negatives"

    def breakers_empty(valid):
        """
        :param valid: dict of valid data for building a disturl

        :returns: list of dicts made from `valid` where each item is
            empty in one of the list elements
        """
        return [{x: ""} for x, _ in list(valid.items())]

    def breakers_trailing_slash(valid):
        """
        :param valid: dict of valid data for building a disturl

        :returns: list of dicts made from `valid` where each item is
            appended trailing slash in one of the list elements
        """
        return  [{x: y+'/'} for x, y in list(valid.items())
                            if not x is "package"]

    breakers = breakers_empty(valid) + breakers_trailing_slash(valid)

    for url in ["nonsense"] + [fmt.format(**merged_dict(valid, x))
    for x in breakers]:
        yield raises(messages.InvalidOBSDistURL)(lambda x: obs.DistURL(x)), url
