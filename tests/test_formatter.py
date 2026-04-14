# TODO: Colorformater tests ?

from mtui.colorlog import create_logger


def test_formatter_name():
    formatter = create_logger("foo")
    assert formatter.name == "foo"


def test_formatter_level():
    formatter = create_logger("foo")
    assert formatter.level == 20

    formatter.setLevel(19)
    assert formatter.level == 19

    assert formatter.getEffectiveLevel() == 19
