from nose.tools import eq_
from mtui.log import create_logger, ColorFormatter

def test_formatter_name():
    formatter = create_logger()
    eq_('mtui', formatter.name)

def test_formatter_level():
    formatter = create_logger()
    eq_(20, formatter.level)

    formatter.setLevel(19)
    eq_(19, formatter.level)

    eq_(19, formatter.getEffectiveLevel())