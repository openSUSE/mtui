import pytest

from mtui.types.rpmver import RPMVersion


@pytest.mark.parametrize(
    ("lower", "higher"),
    [
        ("2014.104.0.0.2svn15878-21.19", "2015.104.0.0.2svn15878-21.12"),
        ("1.2.0-7.20", "1.2.0-7.30"),
        ("0.9~20170329.eb3dfbb", "0.9~20170329.798fdeb"),
    ],
)
def test_version_lt(lower, higher):
    assert RPMVersion(lower) < RPMVersion(higher)


@pytest.mark.parametrize(
    ("lower", "higher"),
    [
        ("2014.104.0.0.2svn15878-21.19", "2015.104.0.0.2svn15878-21.12"),
        ("1.2.0-7.20", "1.2.0-7.30"),
        ("0.9~20170329.eb3dfbb", "0.9~20170329.798fdeb"),
    ],
)
def test_version_gt(lower, higher):
    assert RPMVersion(higher) > RPMVersion(lower)


@pytest.mark.parametrize("version", ["1.2.0-8.1", "0.8+12.ae4"])
def test_version_eq(version):
    assert RPMVersion(version) == RPMVersion(version)


@pytest.mark.parametrize(
    ("higher", "lower"), [("1.2-2", "1.2-2"), ("1.2.3-7.2", "1.2.3-7.2")]
)
def test_version_le(higher, lower):
    assert RPMVersion(lower) <= RPMVersion(higher)


@pytest.mark.parametrize(
    ("higher", "lower"), [("1.2-2", "1.2-2"), ("1.2.3-7.2", "1.2.3-7.2")]
)
def test_version_ge(higher, lower):
    assert RPMVersion(higher) >= RPMVersion(lower)


def test_version_ne():
    assert RPMVersion("1-1.1") != RPMVersion("1-1.2")


def test_version_none():
    with pytest.raises(ValueError):  # noqa: PT011
        RPMVersion(None)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]


def test_version_with_multiple_dashes_splits_on_last():
    """A version field that itself contains a dash splits on the LAST dash.

    Regression: ``rsplit("-")`` without maxsplit raised "too many values to
    unpack" for such strings (reachable via the dpkg/ubuntu querier).
    """
    v = RPMVersion("1.2-3-4")
    assert v.ver == "1.2-3"
    assert v.rel == "4"
    assert str(v) == "1.2-3-4"


@pytest.mark.parametrize(
    ("version", "s"), [("1.2.3-7.3", "1.2.3-7.3"), ("2.3", "2.3"), ("0.8+1-0", "0.8+1")]
)
def test_version_str(version, s):
    assert str(RPMVersion(version)) == s
