"""Translate :mod:`argparse` actions into typed parameters for FastMCP.

FastMCP infers a tool's JSON schema from the wrapper function's Python
signature via :mod:`pydantic`. To synthesise tools from mtui's existing
:class:`argparse.ArgumentParser` definitions we therefore build a real
:class:`inspect.Signature` per command whose parameters carry typing
:class:`~typing.Annotated` hints rich enough for the FastMCP schema
extractor:

* The base Python type — ``str``, ``int``, ``bool``, ``list[str]``,
  ``list[int]`` — comes from ``action.type`` and ``action.nargs``.
* ``action.choices`` becomes a :class:`~typing.Literal` so the schema
  carries an ``enum``.
* ``action.help`` becomes the field description via
  :func:`pydantic.Field`.

This module is intentionally pure: every entry point takes an
:class:`argparse.Action` (or :class:`argparse.ArgumentParser`) and
returns plain data. Side-effect handling (logging, FastMCP
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
    ``None`` (FastMCP renders the field as optional when a default is
    present) while scalars use ``action.default`` directly.
    """
    if nargs is None or nargs == "?":
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
    elif isinstance(nargs, int) and nargs > 0:
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
        # Mutually-exclusive ``--noscript`` / ``--newpackage`` style
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
        # Optional list arg defaults to ``[]`` so the JSON schema stays
        # ``{"type": "array"}`` instead of widening to ``anyOf``.
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
    Each ``dest`` becomes one MCP field; later actions for the same
    ``dest`` are skipped with a WARNING so the caller sees the lossy
    mapping at boot time. Subparser ``dest`` values like ``SUPPRESS``
    are ignored — subparsers are fanned out to multiple tools in
    :mod:`mtui.mcp.tools` rather than collapsed into one parameter.
    """
    required: list[inspect.Parameter] = []
    optional: list[inspect.Parameter] = []
    seen: set[str] = set()
    for action in parser._actions:  # noqa: SLF001 - argparse exposes only this
        if isinstance(action, argparse._SubParsersAction):
            # Handled by the subparser fan-out in mtui.mcp.tools.
            continue
        if action.dest == argparse.SUPPRESS:
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
    return required + optional
