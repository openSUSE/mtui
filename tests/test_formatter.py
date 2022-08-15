# TODO: Colorformater tests ?

from mtui.colorlog import create_logger


def test_formatter_name():
    formatter = create_logger("foo")
    assert "foo" == formatter.name


def test_formatter_level():
    formatter = create_logger("foo")
    assert 20 == formatter.level

    formatter.setLevel(19)
    assert 19 == formatter.level

    assert 19 == formatter.getEffectiveLevel()
