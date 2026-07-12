"""Mutation-killing pins for :mod:`mtui.mcp._schema`.

A full mutmut run left survivors in schema-synthesis code the suite
executes but never asserts on. The tests here pin:

* ``_base_type``: a real ``type=int`` CLI option becomes a JSON-Schema
  ``integer`` (not ``string``), and an unmapped ``type=`` callable falls
  back to ``str`` with a WARNING instead of crashing;
* ``action_to_parameter``: ``store_true``/``store_false``/``store_const``
  defaults (``False``/``True``/``False``) and descriptions, preservation
  of real scalar defaults, optionality of ``nargs='?'`` positionals, and
  ``nargs=N`` array bounds;
* ``_scan_shared_dest_groups``: the group scan continues past trivial /
  mismatched groups (a ``break`` would leave a later shared-dest group
  to the legacy duplicate-dest path) and keys the synthesised parameter
  off the group's FIRST member;
* ``build_parameters``: every special-cased action (subparsers,
  SUPPRESS dest, emit-at, skip, duplicate dest) is skipped with
  ``continue`` semantics so trailing actions still become parameters,
  and de-duplication really tracks names.

Each test was verified to fail against a hand-applied representative
mutant before the pristine code was restored.
"""

from __future__ import annotations

import argparse
import inspect
import logging

import pytest

pytest.importorskip("mcp")

from pydantic import TypeAdapter  # noqa: E402

from mtui.commands import Command  # noqa: E402
from mtui.mcp._schema import (  # noqa: E402
    _base_type,
    action_to_parameter,
    build_parameters,
)


def _params_by_name(command: str) -> dict[str, inspect.Parameter]:
    parser = Command.registry[command].argparser(__import__("sys"))
    return {p.name: p for p in build_parameters(parser)}


def _schema_of(param: inspect.Parameter) -> dict:
    """Render the JSON schema pydantic derives from a parameter annotation."""
    return TypeAdapter(param.annotation).json_schema()


# --------------------------------------------------------------------------- #
# _base_type                                                                  #
# --------------------------------------------------------------------------- #


def test_int_typed_option_maps_to_integer_schema() -> None:
    """``updates --limit`` (``type=int``, ``default=0``) stays an integer.

    Collapsing the type table to ``str`` would advertise a string field
    to every MCP client; dropping the real default would widen the
    schema to nullable.
    """
    params = _params_by_name("updates")
    limit = params["limit"]
    assert limit.default == 0
    assert _schema_of(limit).get("type") == "integer"


def test_unknown_type_callable_falls_back_to_str_with_warning(caplog) -> None:
    """An unmapped ``type=`` callable degrades to ``str`` and warns.

    Inverting the membership test would instead crash with a KeyError
    for unknown types (and route known types through the fallback).
    """

    def bespoke(raw: str) -> str:  # pragma: no cover - never called
        return raw

    parser = argparse.ArgumentParser(prog="probe")
    action = parser.add_argument("--weird", type=bespoke)

    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        result = _base_type(action)

    assert result is str
    assert any("unknown argparse type" in r.message for r in caplog.records)


def test_known_nargs_shapes_produce_no_warning(caplog) -> None:
    """Scalar / ``?`` / ``+`` / REMAINDER shapes never hit the nargs fallback.

    The ``nargs is None or nargs == "?"`` guard degrades gracefully when
    mutated (same return value) but starts logging spurious warnings;
    pin the quiet path.
    """
    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        _params_by_name("export")  # nargs="?" positional
        _params_by_name("updates")  # plain scalars
        _params_by_name("openqa_overview")  # nargs="+" choices
    assert not any("unknown argparse nargs" in r.message for r in caplog.records)


# --------------------------------------------------------------------------- #
# action_to_parameter                                                         #
# --------------------------------------------------------------------------- #


def test_store_true_defaults_false_with_description() -> None:
    """``add_host --keep-mode`` (store_true) defaults to ``False``.

    A flipped default would silently invert the flag for every MCP
    client that omits it.
    """
    params = _params_by_name("add_host")
    keep_mode = params["keep_mode"]
    assert keep_mode.default is False
    field = keep_mode.annotation.__metadata__[0]
    assert field.description == (
        "do not switch to the manual workflow when in automatic mode"
    )


def test_store_false_defaults_true_with_description() -> None:
    """A ``store_false`` action defaults to ``True`` and keeps its help text.

    No mtui command uses ``store_false`` today; pin the generic branch
    directly so the inverted-default mutant cannot hide behind that.
    """
    parser = argparse.ArgumentParser(prog="probe")
    action = parser.add_argument(
        "--no-color", dest="color", action="store_false", help="disable color output"
    )
    result = action_to_parameter(action)
    assert result is not None
    name, annotation, default = result
    assert name == "color"
    assert default is True
    field = annotation.__metadata__[0]
    assert field.description == "disable color output"
    assert TypeAdapter(annotation).json_schema().get("type") == "boolean"


def test_store_const_defaults_false_and_synthesises_description() -> None:
    """``store_const`` flags default to ``False``; help-less ones still get one."""
    params = _params_by_name("update")
    for flag in ("newpackage", "noprepare"):
        assert params[flag].default is False

    parser = argparse.ArgumentParser(prog="probe")
    action = parser.add_argument(
        "--fast", dest="mode", action="store_const", const="fast"
    )
    result = action_to_parameter(action)
    assert result is not None
    name, annotation, default = result
    assert name == "mode"
    assert default is False
    assert annotation.__metadata__[0].description == "sets mode='fast'"


def test_scalar_default_preserved_and_choices_become_enum() -> None:
    """``openqa_overview --days`` keeps ``default=5`` and its 1-30 enum."""
    params = _params_by_name("openqa_overview")
    days = params["days"]
    assert days.default == 5
    schema = _schema_of(days)
    assert schema.get("enum") == list(range(1, 31))


def test_nargs_one_positional_is_required_scalar() -> None:
    """``set_timeout timeout`` (``nargs=1``, ``type=int``) is a required scalar."""
    params = _params_by_name("set_timeout")
    timeout = params["timeout"]
    assert timeout.default is inspect.Parameter.empty  # required
    schema = _schema_of(timeout)
    assert schema.get("type") == "integer"


def test_optional_positional_is_nullable_and_not_required() -> None:
    """``export filename`` (``nargs='?'``) is optional and nullable.

    Mutating the ``("?", "*", REMAINDER)`` membership would make the
    positional required for every MCP client.
    """
    params = _params_by_name("export")
    filename = params["filename"]
    assert filename.default is None
    schema = _schema_of(filename)
    assert {"type": "string"} in schema.get("anyOf", [])
    assert {"type": "null"} in schema.get("anyOf", [])


def test_nargs_plus_list_carries_min_items() -> None:
    """``nargs='+'`` becomes an array with ``minItems=1``."""
    params = _params_by_name("openqa_overview")
    schema = _schema_of(params["aggregated_groups"])
    assert schema.get("type") == "array"
    assert schema.get("minItems") == 1


def test_nargs_n_list_carries_exact_bounds() -> None:
    """``nargs=2`` becomes an array bounded to exactly two items."""
    parser = argparse.ArgumentParser(prog="probe")
    action = parser.add_argument("--pair", nargs=2, help="two values")
    result = action_to_parameter(action)
    assert result is not None
    _, annotation, _ = result
    schema = TypeAdapter(annotation).json_schema()
    assert schema.get("type") == "array"
    assert schema.get("minItems") == 2
    assert schema.get("maxItems") == 2


# --------------------------------------------------------------------------- #
# _scan_shared_dest_groups                                                    #
# --------------------------------------------------------------------------- #


def test_scan_continues_past_trivial_and_mismatched_groups(caplog) -> None:
    """Groups that don't match a recognised shape must not stop the scan.

    A ``break`` on the "fewer than two members" or "distinct dests"
    guard would leave the LAST group unhandled, sending its members
    through the legacy duplicate-dest path (one boolean param + a
    WARNING) instead of one Literal enum.
    """
    parser = argparse.ArgumentParser(prog="probe")
    g1 = parser.add_mutually_exclusive_group()
    g1.add_argument("--solo", action="store_true")  # < 2 members
    g2 = parser.add_mutually_exclusive_group()
    g2.add_argument("--fast", dest="fast", action="store_true")  # distinct
    g2.add_argument("--slow", dest="slow", action="store_true")  # dests
    g3 = parser.add_mutually_exclusive_group()
    g3.add_argument("--enable", dest="mode", action="store_const", const="on")
    g3.add_argument("--disable", dest="mode", action="store_const", const="off")

    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        params = build_parameters(parser)

    names = [p.name for p in params]
    assert names.count("mode") == 1
    (mode,) = (p for p in params if p.name == "mode")
    schema = TypeAdapter(mode.annotation).json_schema()
    enums = [alt.get("enum") for alt in schema.get("anyOf", []) if "enum" in alt]
    assert enums == [["on", "off"]]
    assert not any("duplicate dest" in r.message for r in caplog.records)


def test_scan_emits_group_parameter_at_first_member_position() -> None:
    """The synthesised group parameter lands at the FIRST member's slot.

    With a plain option declared between the two group members, keying
    ``emit_at`` off the second member would reorder the parameter list.
    """
    parser = argparse.ArgumentParser(prog="probe")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--enable", dest="mode", action="store_const", const="on")
    parser.add_argument("--mid")
    group.add_argument("--disable", dest="mode", action="store_const", const="off")

    names = [p.name for p in build_parameters(parser)]
    assert names == ["mode", "mid"]


# --------------------------------------------------------------------------- #
# build_parameters main loop                                                  #
# --------------------------------------------------------------------------- #


def test_build_parameters_processes_actions_after_every_special_case(
    caplog,
) -> None:
    """Trailing actions survive each specially-handled action kind.

    One parser stacks, in order: an ordinary option, a subparsers
    action, a SUPPRESS-dest action, a shared-dest const group (emit-at
    plus skip slots), a genuine duplicate dest, and a final ordinary
    option. Each special case must be skipped with ``continue``
    semantics — any ``break`` drops ``last`` (and friends) from the
    schema; a broken ``seen`` set either duplicates ``dup`` or loses
    the duplicate-dest warning.
    """
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("--first")
    parser.add_subparsers()
    parser.add_argument("--ghost", dest=argparse.SUPPRESS)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--enable", dest="mode", action="store_const", const="on")
    group.add_argument("--disable", dest="mode", action="store_const", const="off")
    parser.add_argument("--dup-a", dest="dup")
    parser.add_argument("--dup-b", dest="dup")
    parser.add_argument("--last")

    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        params = build_parameters(parser)

    names = [p.name for p in params]
    assert names == ["first", "mode", "dup", "last"]
    duplicate_warnings = [
        r for r in caplog.records if "duplicate dest 'dup'" in r.message
    ]
    assert len(duplicate_warnings) == 1
