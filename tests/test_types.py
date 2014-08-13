from nose.tools import eq_, ok_, raises

from mtui.types import MD5Hash

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
