"""Unit tests for ``Command.__init_subclass__`` registration."""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from mtui.commands._command import Command, CommandAlreadyBoundError


@pytest.fixture
def clean_registry() -> Iterator[None]:
    """Snapshot the registry, run the test, then restore it.

    Each test in this module creates throwaway ``Command`` subclasses; without
    this fixture they would leak into ``Command.registry`` for the rest of the
    pytest session and pollute the characterization test in
    ``tests/test_commands_registry.py``.
    """
    snapshot = dict(Command.registry)
    try:
        yield
    finally:
        Command.registry.clear()
        Command.registry.update(snapshot)


def test_subclass_with_command_attribute_is_registered(clean_registry):
    class _Foo(Command):
        command = "tmp_init_subclass_foo"

        def __call__(self) -> None:
            return None

    assert Command.registry["tmp_init_subclass_foo"] is _Foo


def test_subclass_without_command_attribute_is_skipped(clean_registry):
    """Abstract intermediates that do not assign ``command`` are not registered."""
    before = set(Command.registry)

    class _AbstractMid(Command):
        # No ``command`` assignment; this mirrors mtui.commands.apicall.BaseApiCall.
        def __call__(self) -> None:
            return None

    after = set(Command.registry)
    assert before == after
    assert _AbstractMid not in Command.registry.values()


def test_inherited_command_attribute_does_not_re_register(clean_registry):
    """A subclass that does not redeclare ``command`` is skipped even if it inherits one."""

    class _Parent(Command):
        command = "tmp_init_subclass_parent"

        def __call__(self) -> None:
            return None

    class _Child(_Parent):
        # Inherits ``command`` but does not put it in ``__dict__``.
        pass

    assert Command.registry["tmp_init_subclass_parent"] is _Parent
    # The child does not displace the parent's registry slot.
    assert _Child not in Command.registry.values()


def test_duplicate_command_string_raises(clean_registry):
    class _First(Command):
        command = "tmp_init_subclass_dup"

        def __call__(self) -> None:
            return None

    with pytest.raises(CommandAlreadyBoundError, match="tmp_init_subclass_dup"):

        class _Second(Command):
            command = "tmp_init_subclass_dup"

            def __call__(self) -> None:
                return None

    # The first registration survives; the second never makes it in.
    assert Command.registry["tmp_init_subclass_dup"] is _First


def test_aliases_via_subclassing_are_each_registered(clean_registry):
    """Sibling subclasses that each declare ``command`` all register (the quit/exit/EOF case)."""

    class _Base(Command):
        command = "tmp_init_subclass_base"

        def __call__(self) -> None:
            return None

    class _Alias1(_Base):
        command = "tmp_init_subclass_alias1"

    class _Alias2(_Base):
        command = "tmp_init_subclass_alias2"

    assert Command.registry["tmp_init_subclass_base"] is _Base
    assert Command.registry["tmp_init_subclass_alias1"] is _Alias1
    assert Command.registry["tmp_init_subclass_alias2"] is _Alias2


def test_registry_is_a_class_attribute_shared_across_module_imports():
    """The registry is the same dict whether reached via ``Command`` or ``mtui.commands``."""
    from mtui import commands

    assert commands.registry is Command.registry
