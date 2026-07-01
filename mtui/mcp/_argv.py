"""Reassemble argparse argv from a tool-call kwargs dict.

The MCP server delivers a tool call as a dict of ``{dest: value}`` keyed by the
synthesised parameter names from :mod:`mtui.mcp._schema`. To run the
underlying :class:`mtui.commands._command.Command` we must turn that
dict back into a token list ``argparse`` can parse. The mapping is the
inverse of :func:`mtui.mcp._schema.action_to_parameter`:

* ``store_true`` / ``store_false`` / ``store_const`` → emit the long
  flag iff the value is the "on" side.
* ``append`` → emit ``[flag, item]`` per element of the list value, EXCEPT
  when the action's ``nargs`` consumes a remainder/multi
  (``REMAINDER``/``*``/``+``/integer ``N`` — e.g. ``commit -m``, ``lock -c``):
  there the flag is emitted once followed by every token, so argparse rebuilds
  ``[[v1, v2, ...]]`` instead of swallowing a repeated flag as a value.
* Positional ``nargs=REMAINDER``/``*``/``+`` → append elements verbatim
  at the end (after every flag-shaped argument), preserving order.
* Optional ``nargs=REMAINDER`` (e.g. ``reject --message``) → emit the
  flag once followed by every token, but **into the positional tail** so
  it lands after every other flag. A REMAINDER optional consumes all
  remaining argv tokens, so emitting a later ``--flag`` behind it would
  be swallowed as a value; deferring it to the tail keeps it safe
  regardless of ``parser._actions`` declaration order.
* Optional scalar / other multi-value (``+``/``*``/N) → emit
  ``[long_flag, v1, v2, ...]`` (or ``[long_flag, str(value)]`` for a
  scalar) among the flags.
* Positional scalar → append ``str(value)`` after the flags.

The function returns a list of strings; callers (notably
:meth:`McpSession.run_command`) feed it to :func:`shlex.join` and then
to the per-command argparser.
"""

from __future__ import annotations

import argparse
from logging import getLogger
from typing import Any

logger = getLogger("mtui.mcp.argv")

#: Sentinel used by argparse for "no default supplied". We treat
#: kwargs equal to this as "client did not supply anything".
_NO_DEFAULT = object()


def _action_index(
    parser: argparse.ArgumentParser,
) -> dict[str, argparse.Action]:
    """Build a ``{dest: action}`` map for ``parser``.

    Multiple actions can share a ``dest`` (mutually exclusive groups);
    :mod:`mtui.mcp._schema` keeps only the first one, so we mirror that
    here. Used by :func:`kwargs_to_argv` to look up how each kwarg
    should be re-emitted.
    """
    index: dict[str, argparse.Action] = {}
    for action in parser._actions:  # noqa: SLF001 - argparse exposes only this
        if isinstance(action, argparse._SubParsersAction):
            continue
        if action.dest == argparse.SUPPRESS:
            continue
        index.setdefault(action.dest, action)
    return index


def _long_flag(action: argparse.Action) -> str:
    """Return the long ``--flag`` form, falling back to whatever exists.

    Every mtui optional that has a short form also has a long form, but
    we tolerate optional-only short forms defensively.
    """
    for opt in action.option_strings:
        if opt.startswith("--"):
            return opt
    return action.option_strings[0]


def kwargs_to_argv(
    parser: argparse.ArgumentParser,
    kwargs: dict[str, Any],
) -> list[str]:
    """Re-encode a tool-call kwargs dict as argparse-compatible argv.

    Arguments are emitted in the same order ``parser._actions`` defines
    them so the output is deterministic and easy to read in logs. All
    plain optional flags come out first, then the positional tail —
    argparse accepts positionals after flags but not interleaved with
    them when ``nargs=REMAINDER`` is involved. A REMAINDER-consuming
    *optional* (e.g. ``reject --message``) is also deferred to the tail
    so it is emitted after every other flag and cannot swallow one.

    Args:
        parser: The command's :class:`argparse.ArgumentParser`. Used to
            look up the action class behind each kwarg.
        kwargs: ``{dest: value}`` as delivered by the MCP server.

    Returns:
        A list of argv tokens ready to be passed through
        :func:`shlex.join` and back into :meth:`Command.parse_args`.

    """
    index = _action_index(parser)
    flag_tokens: list[str] = []
    positional_tail: list[str] = []

    # ---- synthetic-name routing (load_template -a / -k case) -----------
    # mtui.mcp._schema collapses mutex StoreAction groups that share a
    # dest into one parameter per long flag and attaches the mapping
    # here. Each non-None value is emitted via its underlying action's
    # long flag; the underlying actions are then skipped in the main
    # loop because nothing in ``kwargs`` references their real dest.
    synthetic_map: dict[str, argparse.Action] = getattr(
        parser,
        "_mtui_synthetic_dests",
        {},
    )
    synthetic_action_ids: set[int] = {id(a) for a in synthetic_map.values()}
    for syn_name, syn_action in synthetic_map.items():
        if syn_name not in kwargs:
            continue
        syn_value = kwargs[syn_name]
        if syn_value is None:
            continue
        flag_tokens.extend([_long_flag(syn_action), str(syn_value)])

    # ---- StoreConst enum routing (set_repo -A / -R case) ----------------
    # mtui.mcp._schema also collapses mutex StoreConstAction groups that
    # share a dest into one Literal parameter. The kwarg value is now
    # the chosen const, not a bool, so we walk the parser's mutex
    # groups and emit the long flag of the matching member action.
    # Action ids touched here are skipped by the main loop so the
    # legacy truthy-check below doesn't re-emit a stale flag.
    const_action_ids: set[int] = set()
    for group in parser._mutually_exclusive_groups:  # noqa: SLF001
        members = list(group._group_actions)  # noqa: SLF001
        if len(members) < 2:
            continue
        if not all(
            isinstance(a, argparse._StoreConstAction)
            for a in members  # noqa: SLF001
        ):
            continue
        dests = {a.dest for a in members}
        if len(dests) != 1:
            continue
        shared_dest = members[0].dest
        for a in members:
            const_action_ids.add(id(a))
        if shared_dest not in kwargs:
            continue
        chosen = kwargs[shared_dest]
        if chosen is None:
            continue
        for a in members:
            if a.const == chosen:
                flag_tokens.append(_long_flag(a))
                break

    for action in parser._actions:  # noqa: SLF001 - argparse exposes only this
        if id(action) in synthetic_action_ids or id(action) in const_action_ids:
            continue  # already handled by a synthetic pass above
        if getattr(action, "_mtui_mcp_hidden", False):
            # Never emit a REPL-only hidden flag (e.g. approve/reject --force):
            # build_parameters keeps it out of the MCP schema, and this makes the
            # encoder refuse to render it even if an extra kwarg slips past the
            # SDK's schema validation — the security property no longer depends on
            # Pydantic's default extra='ignore'.
            continue
        if action.dest not in index or index[action.dest] is not action:
            continue  # secondary action sharing a dest (e.g. load_template -k)
        if action.dest not in kwargs:
            continue
        value = kwargs[action.dest]

        # ---- boolean-shaped flags ------------------------------------
        if isinstance(action, argparse._StoreTrueAction):
            if value:
                flag_tokens.append(_long_flag(action))
            continue
        if isinstance(action, argparse._StoreFalseAction):
            if not value:
                flag_tokens.append(_long_flag(action))
            continue
        if isinstance(action, argparse._StoreConstAction):
            if value:
                flag_tokens.append(_long_flag(action))
            continue

        # ---- append ---------------------------------------------------
        if isinstance(action, argparse._AppendAction):
            if not value:
                continue
            flag = _long_flag(action)
            if action.nargs in (argparse.REMAINDER, "*", "+") or isinstance(
                action.nargs, int
            ):
                # append + a remainder/multi nargs (commit -m, lock -c) stores
                # ONE sub-list per flag occurrence and the command reads
                # value[0]. Emit the flag once followed by every token so
                # argparse rebuilds ``[[v1, v2, ...]]``; ``--flag v1 --flag v2``
                # would let REMAINDER swallow the second ``--flag`` as a value,
                # producing ``[[v1, '--flag', v2]]`` and a corrupted message.
                flag_tokens.append(flag)
                flag_tokens.extend(str(item) for item in value)
            else:
                for item in value:
                    flag_tokens.extend([flag, str(item)])
            continue

        # ---- positional ----------------------------------------------
        is_positional = not action.option_strings
        if is_positional:
            if value is None:
                continue
            if isinstance(value, list):
                positional_tail.extend(str(x) for x in value)
            else:
                positional_tail.append(str(value))
            continue

        # ---- optional scalar / multi-value flag ----------------------
        if value is None:
            continue
        flag = _long_flag(action)
        if isinstance(value, list):
            # ``nargs`` in ``+``, ``*``, integer N, or ``REMAINDER``
            # makes argparse consume the rest of the argv after the flag
            # token. We must emit ``[--flag, v1, v2, ...]`` once, not
            # ``[--flag, v1, --flag, v2]`` (the latter would be parsed
            # as a single value list ``[v1, '--flag', v2]``).
            if action.nargs == argparse.REMAINDER:
                # A REMAINDER optional swallows *every* remaining token,
                # including a later ``--flag``. Emitting it inside
                # ``flag_tokens`` is only safe when it happens to be the
                # last flag declared; route it into the positional tail so
                # it is always emitted after every other flag regardless of
                # ``parser._actions`` order (e.g. ``reject --message``).
                positional_tail.append(flag)
                positional_tail.extend(str(item) for item in value)
            else:
                flag_tokens.append(flag)
                flag_tokens.extend(str(item) for item in value)
        else:
            flag_tokens.extend([flag, str(value)])

    return flag_tokens + positional_tail
