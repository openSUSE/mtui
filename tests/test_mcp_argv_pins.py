"""Mutation-killing pins for :func:`mtui.mcp._argv.kwargs_to_argv`.

A full mutmut run left survivors in code paths the existing round-trip
tests execute but never assert on. The tests here pin the behaviours
those mutants change:

* every ``continue`` in the main action loop keeps processing LATER
  actions (a ``break`` there silently drops flags/positionals that
  follow a const-routed group, a secondary shared-dest action, or a
  ``None``-valued kwarg — mis-scoping the command);
* the pre-scan over mutually exclusive groups keeps scanning after a
  trivial/mismatched group so a later const group still routes;
* ``store_false`` emits the flag on the *falsy* side only;
* ``append`` + multi ``nargs`` (``*``/``+``) emits the flag once into
  the positional tail, not once per element;
* subparser / SUPPRESS-dest actions never contribute argv nor stop
  the encoding.

Each test was verified to fail against a hand-applied representative
mutant before the pristine code was restored.
"""

from __future__ import annotations

import argparse

import pytest

pytest.importorskip("mcp")

from mtui.commands import Command  # noqa: E402
from mtui.mcp._argv import kwargs_to_argv  # noqa: E402
from mtui.mcp._schema import build_parameters  # noqa: E402

# --------------------------------------------------------------------------- #
# Const-routed groups must not stop the main loop                             #
# --------------------------------------------------------------------------- #


def test_setrepo_const_group_does_not_stop_later_kwargs() -> None:
    """``set_repo`` emits template and hosts *after* the const-routed group.

    The ``-A``/``-R`` const actions are skipped in the main loop (they
    were already routed by the pre-scan); a ``break`` there instead of
    ``continue`` would silently drop ``--target`` and ``--template``,
    running the command against the wrong scope.
    """
    parser = Command.registry["set_repo"].argparser(__import__("sys"))
    build_parameters(parser)  # attaches the synthetic-routing metadata
    argv = kwargs_to_argv(
        parser,
        {
            "operation": "add",
            "hosts": ["h1", "h2"],
            "template": "SUSE:Maintenance:1:1",
        },
    )
    assert argv == [
        "--add",
        "--target",
        "h1",
        "--target",
        "h2",
        "--template",
        "SUSE:Maintenance:1:1",
    ]
    parsed = parser.parse_args(argv)
    assert parsed.operation == "add"
    assert parsed.hosts == ["h1", "h2"]
    assert parsed.template == "SUSE:Maintenance:1:1"


def test_const_scan_skips_trivial_groups_and_processes_later_ones() -> None:
    """The mutex-group pre-scan must *continue* past non-matching groups.

    Guards for "fewer than two members", "not all StoreConst" and
    "distinct dests" must skip just that group: a ``break`` would leave
    a later const group unrouted and the main loop would then emit the
    group's FIRST flag for any truthy value (``--up`` for ``dir='down'``).
    Same for a const group whose dest is absent or ``None`` in kwargs.
    """
    parser = argparse.ArgumentParser(prog="probe")
    g1 = parser.add_mutually_exclusive_group()
    g1.add_argument("--solo", action="store_true")  # < 2 members
    g2 = parser.add_mutually_exclusive_group()
    g2.add_argument("--alpha", dest="alpha")  # not all StoreConst
    g2.add_argument("--beta", dest="beta")
    g3 = parser.add_mutually_exclusive_group()
    g3.add_argument("--fast", dest="speed", action="store_true")  # distinct
    g3.add_argument("--slow", dest="slowness", action="store_true")  # dests
    g4 = parser.add_mutually_exclusive_group()
    g4.add_argument("--enable", dest="mode", action="store_const", const="on")
    g4.add_argument("--disable", dest="mode", action="store_const", const="off")
    g5 = parser.add_mutually_exclusive_group()
    g5.add_argument("--up", dest="direction", action="store_const", const="up")
    g5.add_argument("--down", dest="direction", action="store_const", const="down")

    # First const group present-but-None, second supplied: only the
    # second may emit, and it must emit the flag matching the const.
    assert kwargs_to_argv(parser, {"mode": None, "direction": "down"}) == ["--down"]
    # Absent first const dest, supplied second. The chosen const must
    # differ from the group's FIRST member: a break-instead-of-continue
    # mutant leaves g5 unrouted and the legacy truthy fallback emits the
    # first flag (--up) regardless of the value -- "down" catches that.
    assert kwargs_to_argv(parser, {"direction": "down"}) == ["--down"]
    assert kwargs_to_argv(parser, {"direction": "up"}) == ["--up"]
    # Plain routing sanity for the first const group.
    assert kwargs_to_argv(parser, {"mode": "off"}) == ["--disable"]


# --------------------------------------------------------------------------- #
# Main loop continues past skipped/None-valued actions                        #
# --------------------------------------------------------------------------- #


def test_secondary_shared_dest_action_does_not_stop_loop() -> None:
    """A second action sharing a dest is skipped; later actions still emit.

    Without the schema pre-scan (raw parser), only the FIRST action of a
    shared dest re-emits the kwarg. The secondary action's ``continue``
    must not terminate the loop — the ``--after`` flag declared later
    must still be encoded.
    """
    parser = argparse.ArgumentParser(prog="probe")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("-a", "--alpha", dest="val")
    group.add_argument("-b", "--beta", dest="val")
    parser.add_argument("-x", "--after", action="store_true")

    argv = kwargs_to_argv(parser, {"val": "V", "after": True})
    assert argv == ["--alpha", "V", "--after"]


def test_none_positional_does_not_stop_loop() -> None:
    """An unsupplied (``None``) optional positional must not eat the tail."""
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("maybe", nargs="?")
    parser.add_argument("rest", nargs="*")

    argv = kwargs_to_argv(parser, {"maybe": None, "rest": ["r1", "r2"]})
    assert argv == ["r1", "r2"]


def test_none_optional_flag_does_not_stop_loop() -> None:
    """A ``None``-valued optional flag must not eat later positionals."""
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("-o", "--opt")
    parser.add_argument("pos", nargs="*")

    argv = kwargs_to_argv(parser, {"opt": None, "pos": ["a", "b"]})
    assert argv == ["a", "b"]


# --------------------------------------------------------------------------- #
# store_false / append-multi generic branches                                 #
# --------------------------------------------------------------------------- #


def test_store_false_emits_flag_only_when_value_falsy() -> None:
    """``store_false``: flag iff the kwarg is the "off" side; tail survives."""
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("--no-color", dest="color", action="store_false")
    parser.add_argument("tail", nargs="*")

    # False -> flag emitted, and the loop continues to the tail.
    assert kwargs_to_argv(parser, {"color": False, "tail": ["t1"]}) == [
        "--no-color",
        "t1",
    ]
    # True (the default side) -> no flag.
    assert kwargs_to_argv(parser, {"color": True, "tail": ["t1"]}) == ["t1"]
    parsed = parser.parse_args(["--no-color"])
    assert parsed.color is False


@pytest.mark.parametrize("nargs", ["*", "+"])
def test_append_multi_nargs_emits_flag_once_in_tail(nargs: str) -> None:
    """``append`` + ``nargs='*'``/``'+'`` re-emits one flag then every token.

    Emitting ``--words a --words b`` instead would make argparse rebuild
    ``[['a'], ['b']]`` (or swallow the repeated flag), corrupting the
    value; the flag+tokens must also land in the positional tail, i.e.
    after ordinary flags.
    """
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("--words", action="append", nargs=nargs)
    parser.add_argument("-x", "--xflag", action="store_true")

    argv = kwargs_to_argv(parser, {"words": ["a", "b"], "xflag": True})
    assert argv == ["--xflag", "--words", "a", "b"]
    parsed = parser.parse_args(argv)
    assert parsed.words == [["a", "b"]]


def test_short_only_option_falls_back_to_short_flag() -> None:
    """An option with only a short form is emitted with that short form."""
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("-z", dest="z")

    assert kwargs_to_argv(parser, {"z": "v"}) == ["-z", "v"]


# --------------------------------------------------------------------------- #
# Subparser / SUPPRESS actions and synthetic routing                           #
# --------------------------------------------------------------------------- #


def test_subparser_and_suppress_actions_are_ignored_not_fatal() -> None:
    """Subparser and SUPPRESS-dest actions contribute nothing and don't stop.

    The dest index must skip both (a ``break`` there would empty the
    index and drop every later kwarg), and the main loop must tolerate
    an action whose dest is missing from the index without a KeyError.
    """
    parser = argparse.ArgumentParser(prog="probe")
    parser.add_subparsers()
    parser.add_argument("--ghost", dest=argparse.SUPPRESS)
    parser.add_argument("--flag")

    assert kwargs_to_argv(parser, {"flag": "v"}) == ["--flag", "v"]
    assert kwargs_to_argv(parser, {}) == []


def test_loadtemplate_synthetic_routing_ignores_underlying_dest() -> None:
    """Once synthetic routing owns a group, the raw dest kwarg is inert.

    The main loop must skip the underlying actions by identity; keying
    the skip-set off anything else would re-emit ``--auto-review-id``
    with the stray ``update`` value appended after the synthetic pass.
    """
    parser = Command.registry["load_template"].argparser(__import__("sys"))
    build_parameters(parser)
    argv = kwargs_to_argv(
        parser,
        {"auto_review_id": "SUSE:Maintenance:1:1", "update": "SUSE:Maintenance:9:9"},
    )
    assert argv == ["--auto-review-id", "SUSE:Maintenance:1:1"]


def test_loadtemplate_kernel_id_alone_still_emits() -> None:
    """A synthetic kwarg that is simply absent must not stop the synthetic pass.

    ``kernel_review_id`` is the SECOND entry of the synthetic map; when
    ``auto_review_id`` is not in kwargs at all (as opposed to ``None``),
    the pass must continue to the kernel entry.
    """
    parser = Command.registry["load_template"].argparser(__import__("sys"))
    build_parameters(parser)
    argv = kwargs_to_argv(parser, {"kernel_review_id": "SUSE:Maintenance:2:2"})
    assert argv == ["--kernel-review-id", "SUSE:Maintenance:2:2"]
