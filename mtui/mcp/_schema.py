"""Translate :mod:`argparse` actions into typed parameters for the MCP server.

:mod:`mcp.server.fastmcp` infers a tool's JSON schema from the wrapper
function's Python signature via :mod:`pydantic`. To synthesise tools
from mtui's existing :class:`argparse.ArgumentParser` definitions we
therefore build a real :class:`inspect.Signature` per command whose
parameters carry typing :class:`~typing.Annotated` hints rich enough
for the SDK's schema extractor:

* The base Python type — ``str``, ``int``, ``bool``, ``list[str]``,
  ``list[int]`` — comes from ``action.type`` and ``action.nargs``.
* ``action.choices`` becomes a :class:`~typing.Literal` so the schema
  carries an ``enum``.
* ``action.help`` becomes the field description via
  :func:`pydantic.Field`.

This module is intentionally pure: every entry point takes an
:class:`argparse.Action` (or :class:`argparse.ArgumentParser`) and
returns plain data. Side-effect handling (logging, MCP-server tool
registration) lives in :mod:`mtui.mcp.tools`.
"""

from __future__ import annotations

import argparse
import inspect
from logging import getLogger
from pathlib import Path
from typing import Annotated, Any, Literal

from pydantic import Field

from ..types.updateid import AutoOBSUpdateID, KernelOBSUpdateID

logger = getLogger("mtui.mcp.schema")

#: Sentinel default for action types we cannot translate. Treated as
#: "required" in :func:`build_parameters` because we have no other way
#: to express "the caller must provide a value".
_REQUIRED = inspect.Parameter.empty

#: Argparse callables we explicitly understand. ``type=`` callables not
#: in this table fall back to :class:`str` with a WARNING.
_TYPE_MAP: dict[Any, type] = {
    None: str,  # argparse default when ``type=`` is omitted
    str: str,
    int: int,
    Path: str,
    AutoOBSUpdateID: str,
    KernelOBSUpdateID: str,
}

#: Extra description fragments appended for our exotic types so the
#: client sees what shape a string field actually expects.
_TYPE_DESCRIPTIONS: dict[Any, str] = {
    AutoOBSUpdateID: (
        "OBS/IBS update id, e.g. ``SUSE:Maintenance:1234:567890`` or a "
        "QEM auto-review id like ``S:1234:567890``."
    ),
    KernelOBSUpdateID: (
        "Kernel-specific OBS/IBS update id, accepts the same shapes as "
        "the generic update id."
    ),
}


def _base_type(action: argparse.Action) -> type:
    """Return the Python scalar type a single argparse token decodes to.

    Booleans are handled separately by the action-class dispatch in
    :func:`action_to_parameter` so this function only deals with the
    ``type=`` callable.
    """
    t = action.type
    if t in _TYPE_MAP:
        return _TYPE_MAP[t]
    logger.warning(
        "unknown argparse type %r for argument %r; falling back to str",
        t,
        action.dest,
    )
    return str


def _wrap_nargs(base: Any, nargs: object) -> tuple[Any, bool]:
    """Apply ``nargs`` semantics to ``base``.

    Returns ``(annotation, is_list)``. ``is_list`` is reported so the
    caller can decide on the default — list-shaped fields default to
    ``None`` (the MCP server renders the field as optional when a default is
    present) while scalars use ``action.default`` directly.
    """
    if nargs is None or nargs == "?":
        return base, False
    # ``nargs=1`` is an argparse quirk: it parses exactly one token but
    # wraps the result in a 1-element list. For the MCP schema we expose
    # a plain scalar so clients send ``"prague"`` instead of
    # ``["prague"]``; the ``_argv`` encoder accepts a scalar positional
    # and argparse still produces the 1-list internally.
    if nargs == 1:
        return base, False
    # ``argparse.REMAINDER`` is the sentinel "..." string at runtime;
    # treat it like ``"*"`` for schema purposes.
    if nargs in ("*", "+", argparse.REMAINDER) or isinstance(nargs, int):
        return list[base], True
    logger.warning("unknown argparse nargs %r; treating as scalar string", nargs)
    return base, False


def _field(
    *,
    description: str | None,
    choices: object,
    nargs: object,
) -> Any:
    """Build a :class:`pydantic.Field` with the right metadata.

    ``choices`` plus ``nargs="+"`` would imply ``minItems=1`` on the
    array; that is recorded here for the few commands that use the
    combination (``openqa_overview --aggregated-groups`` today).
    """
    kw: dict[str, Any] = {}
    if description:
        kw["description"] = description.strip()
    if nargs == "+":
        kw["min_length"] = 1
    elif isinstance(nargs, int) and nargs > 1:
        # ``nargs=1`` is intentionally excluded: ``_wrap_nargs`` exposes
        # it as a scalar, where ``min_length``/``max_length`` would be
        # interpreted as string-length bounds instead of array bounds.
        kw["min_length"] = nargs
        kw["max_length"] = nargs
    return Field(**kw)


def action_to_parameter(
    action: argparse.Action,
) -> tuple[str, Any, Any] | None:
    """Translate one :class:`argparse.Action` into a callable parameter.

    Returns ``(name, annotation, default)`` where ``annotation`` is an
    :class:`~typing.Annotated` type ready to be hung on a synthesised
    function signature. ``default`` is :data:`_REQUIRED` for required
    arguments and the real default otherwise.

    Help and version actions return ``None``; the MCP layer advertises
    descriptions via the tool envelope, so per-call ``--help`` is
    redundant.
    """
    if isinstance(action, argparse._HelpAction | argparse._VersionAction):
        return None

    name = action.dest
    help_text = action.help

    # ------------------------------------------------------------- boolean
    if isinstance(action, argparse._StoreTrueAction):
        return (
            name,
            Annotated[bool, _field(description=help_text, choices=None, nargs=None)],
            False,
        )
    if isinstance(action, argparse._StoreFalseAction):
        return (
            name,
            Annotated[bool, _field(description=help_text, choices=None, nargs=None)],
            True,
        )
    if isinstance(action, argparse._StoreConstAction):
        # Mutually-exclusive ``--newpackage`` / ``--noprepare`` style
        # flags share a dest in some commands but mtui's usage gives each
        # its own dest, so a flat boolean is faithful.
        desc = (help_text or "").strip()
        if desc:
            desc = f"{desc} (sets {name}={action.const!r})"
        else:
            desc = f"sets {name}={action.const!r}"
        return name, Annotated[bool, Field(description=desc)], False

    # ------------------------------------------------------------- list
    if isinstance(action, argparse._AppendAction):
        base = _base_type(action)
        desc = (help_text or "").strip()
        if base in _TYPE_DESCRIPTIONS:
            desc = f"{desc} {_TYPE_DESCRIPTIONS[base]}".strip()
        list_type = list[base]  # ty: ignore[invalid-type-form]
        ann = Annotated[
            list_type,
            _field(
                description=desc or None, choices=action.choices, nargs=action.nargs
            ),
        ]
        # Optional list arg defaults to ``[]`` so the JSON schema stays
        # ``{"type": "array"}`` instead of widening to ``anyOf``.
        return name, ann, []

    # ------------------------------------------------------------- store
    base = _base_type(action)
    annotated_base: object
    if action.choices is not None:
        # Render choices as a Literal so the JSON schema carries an
        # ``enum``. Convert ``range`` and other lazy iterables eagerly.
        choices = tuple(action.choices)
        annotated_base = Literal[choices]  # ty: ignore[invalid-type-form]
    else:
        annotated_base = base

    annotation, is_list = _wrap_nargs(annotated_base, action.nargs)

    desc = (help_text or "").strip()
    if action.type in _TYPE_DESCRIPTIONS:
        desc = f"{desc} {_TYPE_DESCRIPTIONS[action.type]}".strip()

    field = _field(description=desc or None, choices=action.choices, nargs=action.nargs)

    # Required vs optional. Positionals are required unless nargs makes
    # them optional. Optionals are required iff explicitly marked.
    is_positional = not action.option_strings
    if is_positional:
        required = action.nargs not in ("?", "*", argparse.REMAINDER)
    else:
        required = bool(action.required)

    if required:
        default: Any = _REQUIRED
        annotated = Annotated[annotation, field]
    elif is_list:
        # Optional list arg: preserve a real non-empty argparse default
        # (e.g. ``openqa_overview --aggregated-groups`` defaults to
        # ``["core"]``) so the MCP path matches the REPL. An empty
        # default falls back to ``[]`` so the JSON schema stays
        # ``{"type": "array"}`` instead of widening to ``anyOf``.
        if isinstance(action.default, list | tuple) and action.default:
            default = list(action.default)
        else:
            default = []
        annotated = Annotated[annotation, field]
    else:
        default = action.default
        # Scalars with ``default=None`` get ``X | None`` so the
        # schema reflects nullability rather than failing validation.
        if default is None:
            annotated = Annotated[annotation | None, field]
        else:
            annotated = Annotated[annotation, field]

    return name, annotated, default


def _synthetic_name(action: argparse.Action) -> str:
    """Derive a synthetic kwarg name from an action's long option.

    Used when several actions in a mutually exclusive group share a
    single ``dest`` but represent semantically distinct CLI inputs (the
    ``load_template`` ``-a`` / ``-k`` case): collapsing them into one
    parameter hides half the surface from MCP clients, so each gets
    its own parameter named after its long flag.

    ``--auto-review-id`` becomes ``auto_review_id``. Falls back to the
    first option string with non-alphanumeric characters mapped to
    underscores when no long form exists.
    """
    for opt in action.option_strings:
        if opt.startswith("--"):
            return opt[2:].replace("-", "_")
    return action.option_strings[0].lstrip("-").replace("-", "_")


def _scan_shared_dest_groups(
    parser: argparse.ArgumentParser,
) -> tuple[
    dict[int, list[tuple[str, Any, Any]]],
    set[int],
    dict[str, argparse.Action],
]:
    """Pre-scan mutually exclusive groups whose members share a ``dest``.

    Returns three structures keyed off action ``id()``:

    * ``emit_at`` — action id of the *first* member of each handled
      group, mapped to the synthesised ``(name, annotation, default)``
      tuple(s) to emit when that action's slot is reached in the main
      loop. For the ``StoreConstAction`` (``set_repo``) shape this is
      a single tuple — one ``Literal`` enum. For the ``StoreAction``
      (``load_template``) shape this is the *first* of N tuples; the
      rest are stored alongside so they emit in deterministic order.
    * ``skip`` — action ids to silently skip in the main loop (every
      member of a handled group lands here so no ``duplicate dest``
      warning fires).
    * ``synthetic_map`` — synthetic-name -> action map written onto
      the parser so :mod:`mtui.mcp._argv` can route the kwarg back to
      the right CLI flag.

    Groups that don't fit one of the two recognised shapes are left
    alone; the main loop's existing first-wins + WARNING path handles
    them.
    """
    emit_at: dict[int, list[tuple[str, Any, Any]]] = {}
    skip: set[int] = set()
    synthetic_map: dict[str, argparse.Action] = {}

    for group in parser._mutually_exclusive_groups:  # noqa: SLF001
        members = list(group._group_actions)  # noqa: SLF001
        if len(members) < 2:
            continue
        dests = {a.dest for a in members}
        if len(dests) != 1:
            continue  # distinct dests — main loop handles each as-is

        first = members[0]

        # ---- shape A: StoreConstAction mutex -> single Literal enum
        if all(isinstance(a, argparse._StoreConstAction) for a in members):  # noqa: SLF001
            consts = tuple(a.const for a in members)
            descs = [
                f"``{a.option_strings[-1]}``: {(a.help or '').strip()}".rstrip(": ")
                for a in members
            ]
            description = "Operation selector. " + " ".join(descs)
            annotation = Annotated[
                Literal[consts],  # ty: ignore[invalid-type-form]
                Field(description=description),
            ]
            default: Any = _REQUIRED if group.required else None
            if default is None:
                annotation = Annotated[
                    Literal[consts] | None,  # ty: ignore[invalid-type-form]
                    Field(description=description),
                ]
            emit_at[id(first)] = [(first.dest, annotation, default)]
            for a in members:
                skip.add(id(a))
            continue

        # ---- shape B: StoreAction mutex -> N synthetic-name params
        if all(
            isinstance(a, argparse._StoreAction)  # noqa: SLF001
            and not isinstance(a, argparse._StoreConstAction)  # noqa: SLF001
            for a in members
        ):
            tuples: list[tuple[str, Any, Any]] = []
            sibling_flags = ", ".join(
                a.option_strings[-1] for a in members if a.option_strings
            )
            for a in members:
                syn = _synthetic_name(a)
                if syn in synthetic_map:
                    # Defensive: refuse to collide; fall back to
                    # legacy first-wins behaviour for this group.
                    logger.warning(
                        "synthetic name %r collides in parser %r; "
                        "leaving mutex group %r untouched",
                        syn,
                        parser.prog,
                        sibling_flags,
                    )
                    tuples = []
                    break
                base = _base_type(a)
                desc = (a.help or "").strip()
                if a.type in _TYPE_DESCRIPTIONS:
                    desc = f"{desc} {_TYPE_DESCRIPTIONS[a.type]}".strip()
                if group.required:
                    desc = (
                        f"{desc} (mutually exclusive with {sibling_flags}; "
                        "exactly one of the group is required)"
                    )
                else:
                    desc = f"{desc} (mutually exclusive with {sibling_flags})"
                # ``Annotated[X | None, ...]`` makes the MCP server schema
                # nullable. Widen to ``Any`` locally so ty does not flag
                # the union expression as a runtime type value (same
                # pattern as the optional-scalar branch above).
                optional_base: Any = base
                annotation = Annotated[optional_base | None, Field(description=desc)]
                tuples.append((syn, annotation, None))
                synthetic_map[syn] = a

            if tuples:
                emit_at[id(first)] = tuples
                for a in members:
                    skip.add(id(a))

    return emit_at, skip, synthetic_map


def build_parameters(
    parser: argparse.ArgumentParser,
) -> list[inspect.Parameter]:
    """Walk a parser's actions and return ordered :class:`inspect.Parameter`s.

    The order is: required arguments first (in the order argparse
    registered them), then optionals. This is what
    :class:`inspect.Signature` requires — parameters without defaults
    must precede those with defaults.

    Multiple argparse actions can share a single ``dest`` (mutually
    exclusive groups in ``load_template`` and ``set_repo`` do this).
    Two shapes are handled specially via :func:`_scan_shared_dest_groups`:

    * All members are ``_StoreConstAction`` (``set_repo`` ``-A`` / ``-R``)
      → one ``Literal`` enum parameter covering both consts.
    * All members are ``_StoreAction`` with distinct ``type=`` callables
      (``load_template`` ``-a`` / ``-k``) → one synthetic-name parameter
      per action, plus a parser-attached map so :mod:`mtui.mcp._argv`
      can route each kwarg back to the right flag.

    Anything else falls through to the legacy first-wins-with-WARNING
    path. Subparser ``dest`` values like ``SUPPRESS`` are ignored —
    subparsers are fanned out to multiple tools in :mod:`mtui.mcp.tools`
    rather than collapsed into one parameter. Actions carrying a truthy
    ``_mtui_mcp_hidden`` attribute are likewise skipped, letting a
    command keep an argparse flag out of its MCP schema.
    """
    emit_at, skip, synthetic_map = _scan_shared_dest_groups(parser)
    # Expose the synthetic-name map for the argv encoder and the
    # MCP wrapper's "exactly one required" runtime check.
    parser._mtui_synthetic_dests = synthetic_map  # type: ignore[attr-defined]  # noqa: SLF001 - attach metadata, see mtui.mcp._argv  # ty: ignore[unresolved-attribute]

    required: list[inspect.Parameter] = []
    optional: list[inspect.Parameter] = []
    seen: set[str] = set()

    def _add(name: str, annotation: Any, default: Any) -> None:
        if name in seen:
            return
        seen.add(name)
        if default is _REQUIRED:
            required.append(
                inspect.Parameter(
                    name,
                    inspect.Parameter.KEYWORD_ONLY,
                    annotation=annotation,
                )
            )
        else:
            optional.append(
                inspect.Parameter(
                    name,
                    inspect.Parameter.KEYWORD_ONLY,
                    annotation=annotation,
                    default=default,
                )
            )

    for action in parser._actions:  # noqa: SLF001 - argparse exposes only this
        if isinstance(action, argparse._SubParsersAction):
            # Handled by the subparser fan-out in mtui.mcp.tools.
            continue
        if action.dest == argparse.SUPPRESS:
            continue
        if getattr(action, "_mtui_mcp_hidden", False):
            # Actions tagged ``_mtui_mcp_hidden`` are internal-only flags
            # that must not surface as MCP tool parameters.
            continue

        if id(action) in emit_at:
            for name, annotation, default in emit_at[id(action)]:
                _add(name, annotation, default)
            continue
        if id(action) in skip:
            continue

        result = action_to_parameter(action)
        if result is None:
            continue
        name, annotation, default = result
        if name in seen:
            logger.warning(
                "duplicate dest %r in parser %r; ignoring action %r",
                name,
                parser.prog,
                action.option_strings or "<positional>",
            )
            continue
        _add(name, annotation, default)

    return required + optional
