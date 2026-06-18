"""SMELT query commands, auto-exposed over MCP as ``mcp__mtui__smelt_*``.

* ``smelt_update``  — SMELT detail (priority, deadline, status, …) for the
  loaded update; dispatches SLFO -> REST v2, Maintenance -> GraphQL.
* ``smelt_checkers`` — checker (build-check) result runs for the loaded SLFO
  update.
* ``smelt_updates`` — enumerate the SLFO update queue with filters, e.g. the
  testing updates still pending a review group.

All are read-only and require ``[smelt] url`` to be configured.
"""

from __future__ import annotations

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..data_sources import Smelt
from ..data_sources.smelt import slfo_update_id
from ..support.misc import requires_update
from ..types import RequestKind
from . import Command

logger = getLogger("mtui.commands.smelt")


def _smelt(cmd: Command) -> Smelt | None:
    """Build the client; print a hint and return ``None`` when unconfigured."""
    smelt = Smelt(cmd.config)
    if not smelt.configured:
        cmd.println("SMELT not configured — set [smelt] url in your mtui config")
        return None
    return smelt


def _loaded_slfo_id(metadata) -> str | None:
    """REST v2 update id for the loaded update, or ``None`` if not SLFO."""
    rrid = metadata.rrid
    if rrid.kind is not RequestKind.SLFO:
        return None
    return slfo_update_id(getattr(metadata, "giteapr", None), rrid.review_id)


class SmeltUpdate(Command):
    """Show the loaded update's SMELT detail (priority, deadline, status, …)."""

    command = "smelt_update"

    @requires_update
    def __call__(self) -> None:
        """Print SMELT detail for the loaded update."""
        smelt = _smelt(self)
        if smelt is None:
            return
        rrid = self.metadata.rrid
        if rrid.kind is RequestKind.SLFO:
            uid = _loaded_slfo_id(self.metadata)
            data = smelt.update(uid) if uid else None
            if not data:
                self.println("no SMELT data for this update")
                return
            self.println(f"id       : {data.get('human_readable_id')}")
            self.println(f"status   : {data.get('status')}")
            self.println(f"category : {data.get('category')}")
            self.println(f"rating   : {(data.get('rating') or {}).get('name')}")
            self.println(f"priority : {data.get('priority')}")
            self.println(f"deadline : {data.get('deadline')}")
            pkgs = [
                p.get("name")
                for p in (data.get("packages") or [])
                if isinstance(p, dict)
            ]
            self.println(f"packages : {', '.join(pkgs)}")
        elif rrid.kind is RequestKind.MAINTENANCE:
            node = smelt.incident(rrid.maintenance_id)
            if not node:
                self.println("no SMELT incident data for this update")
                return
            self.println(f"incident : {node.get('incidentId')}")
            self.println(f"status   : {(node.get('status') or {}).get('name')}")
            self.println(f"rating   : {(node.get('rating') or {}).get('name')}")
            self.println(f"priority : {node.get('priority')}")
            self.println(f"crd      : {node.get('crd')}")
        else:
            self.println("SMELT detail is not available for this request kind")


class SmeltCheckers(Command):
    """Show checker (build-check) result runs for the loaded SLFO update."""

    command = "smelt_checkers"

    @requires_update
    def __call__(self) -> None:
        """Print the checker-result runs for the loaded update."""
        smelt = _smelt(self)
        if smelt is None:
            return
        uid = _loaded_slfo_id(self.metadata)
        if not uid:
            self.println("checker results are available for SLFO updates only")
            return
        runs = smelt.checker_results(uid)
        if not runs:
            self.println("no checker results for this update")
            return
        for r in runs:
            self.println(
                f"{str(r.get('checker_type', '?')):10} "
                f"pass={r.get('pass_count', 0)} fail={r.get('fail_count', 0)} "
                f"warn={r.get('warn_count', 0)} error={r.get('error_count', 0)} "
                f"running={r.get('running_count', 0)}  "
                f"{r.get('finished') or r.get('started') or ''}"
            )


class SmeltUpdates(Command):
    """Enumerate the SLFO update queue (with filters)."""

    command = "smelt_updates"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register filters for the queue listing."""
        parser.add_argument(
            "--status", help="only updates with this status, e.g. testing"
        )
        parser.add_argument(
            "--review-group",
            dest="review_group",
            help="narrow to updates assigned to this review group",
        )
        parser.add_argument(
            "--pending",
            help="only updates whose review by this group is not yet APPROVED",
        )
        parser.add_argument(
            "--limit", type=int, default=0, help="cap the number of rows (0 = all)"
        )

    def __call__(self) -> None:
        """Print the filtered SLFO update queue, highest priority first."""
        smelt = _smelt(self)
        if smelt is None:
            return
        items = smelt.unreleased(self.args.review_group)
        out = []
        for it in items:
            if self.args.status and it.get("status") != self.args.status:
                continue
            if self.args.pending:
                rev = next(
                    (
                        r
                        for r in (it.get("reviews") or [])
                        if r.get("name") == self.args.pending
                    ),
                    None,
                )
                if not rev or rev.get("state") == "APPROVED":
                    continue
            out.append(it)
        out.sort(key=lambda x: -(x.get("priority") or 0))
        if self.args.limit:
            out = out[: self.args.limit]
        for it in out:
            pkgs = [
                p.get("name") for p in (it.get("packages") or []) if isinstance(p, dict)
            ]
            self.println(
                f"{str(it.get('human_readable_id', '')):20} "
                f"{str(it.get('status')):16} prio={it.get('priority')}  "
                f"{','.join(pkgs)[:50]}"
            )
        self.println(f"\n{len(out)} update(s)")
