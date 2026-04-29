"""Unit tests for the OpenQAResults dataclass."""

from unittest.mock import MagicMock

from mtui.types import OpenQAResult, OpenQAResults


def _falsy_result() -> MagicMock:
    """A connector-like mock whose __bool__ is False."""
    m = MagicMock()
    m.__bool__.return_value = False
    return m


def _truthy_result() -> MagicMock:
    """A connector-like mock whose __bool__ is True."""
    m = MagicMock()
    m.__bool__.return_value = True
    return m


class TestOpenQAResultsDefaults:
    def test_defaults_are_none_and_empty_list(self) -> None:
        r = OpenQAResults()
        assert r.auto is None
        assert r.kernel == []

    def test_kernel_default_is_distinct_per_instance(self) -> None:
        # Guard against mutable-default footgun
        a = OpenQAResults()
        b = OpenQAResults()
        a.kernel.append(_truthy_result())
        assert b.kernel == []


class TestOpenQAResultsBool:
    def test_empty_is_falsy(self) -> None:
        assert not OpenQAResults()

    def test_truthy_auto_makes_truthy(self) -> None:
        assert OpenQAResults(auto=_truthy_result())

    def test_falsy_auto_alone_is_falsy(self) -> None:
        assert not OpenQAResults(auto=_falsy_result())

    def test_truthy_kernel_makes_truthy(self) -> None:
        assert OpenQAResults(kernel=[_truthy_result()])

    def test_kernel_with_only_falsy_is_falsy(self) -> None:
        assert not OpenQAResults(kernel=[_falsy_result(), _falsy_result()])

    def test_truthy_kernel_among_falsy_makes_truthy(self) -> None:
        assert OpenQAResults(kernel=[_falsy_result(), _truthy_result()])


class TestOpenQAResultsMutation:
    def test_assign_auto(self) -> None:
        r = OpenQAResults()
        item = _truthy_result()
        r.auto = item
        assert r.auto is item

    def test_append_to_kernel(self) -> None:
        r = OpenQAResults()
        a, b = _truthy_result(), _truthy_result()
        r.kernel.append(a)
        r.kernel.append(b)
        assert r.kernel == [a, b]


class TestOpenQAResultProtocol:
    def test_protocol_is_runtime_checkable(self) -> None:
        # MagicMock satisfies any structural protocol via attribute access.
        m = _truthy_result()
        # mark required attribute
        m.kind = "auto"
        assert isinstance(m, OpenQAResult)
