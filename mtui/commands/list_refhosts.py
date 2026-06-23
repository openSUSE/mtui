"""The ``list_refhosts`` command: query/search the refhost inventory.

Reads the refhost inventory (the same source ``add_host`` resolves through
:data:`~mtui.hosts.refhost.RefhostsFactory`) and prints matching hosts
**without connecting** — no SSH, no lock, no loaded template required. This
lets fleet maintenance and manual users find refhosts through mtui instead of
parsing ``refhosts.yml`` by hand.

Location is **not** used to scope the search by default (it is being retired):
every location is searched and results are de-duplicated by host name. The
optional ``--free`` flag additionally connects to the matched hosts to report
their live mtui-lock state (the only part that goes on the wire).
"""

import concurrent.futures
import contextlib
import json
from logging import getLogger

from ..cli.completion import complete_choices
from ..hosts.refhost import Attributes, RefhostsFactory
from ..hosts.target import Target
from ..support.concurrency import ContextExecutor
from . import Command

logger = getLogger("mtui.commands.list_refhosts")


class ListRefhosts(Command):
    """Lists reference hosts from refhosts.yml (offline search, no connect).

    With no filters every known refhost is listed. Filter by hostname glob,
    arch, base product, version, or addon — or pass a full ``--testplatform``
    query (same syntax ``add_host`` matches on). ``--pool`` groups the result
    by test-target slot (one candidate per arch/codestream). Location is
    ignored by default.
    """

    command = "list_refhosts"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-T",
            "--testplatform",
            help="match a SMELT testplatform query "
            "(e.g. 'base=sles(major=15,minor=6);arch=[x86_64]')",
        )
        parser.add_argument(
            "-n", "--name", help="hostname glob, e.g. 'whale-*' or '*.qam.suse.cz'"
        )
        parser.add_argument(
            "-a",
            "--arch",
            action="append",
            help="arch filter (repeatable): x86_64 / aarch64 / ppc64le / s390x",
        )
        parser.add_argument(
            "-p", "--product", help="base-product substring, e.g. sles / sled / SLE_HPC"
        )
        parser.add_argument(
            "--version", help="product version: 15-SP6 / 15.6 / 15 (SP optional)"
        )
        parser.add_argument(
            "--addon", action="append", help="addon-name substring (repeatable)"
        )
        parser.add_argument(
            "-l",
            "--location",
            help="restrict to a single location (default: search all locations)",
        )
        parser.add_argument(
            "--pool",
            action="store_true",
            help="group by test-target slot (product+version+arch+addons)",
        )
        parser.add_argument(
            "--json", action="store_true", dest="as_json", help="emit JSON"
        )
        parser.add_argument(
            "--free",
            action="store_true",
            help="also probe live mtui-lock state (connects to each matched host)",
        )
        parser.add_argument(
            "-v", "--verbose", action="store_true", help="include addons in the output"
        )

    @staticmethod
    def _ver_str(version) -> str:
        if version is None:
            return ""
        if version.minor is None or version.minor == "":
            return str(version.major)
        return f"{version.major}-{version.minor}"

    @classmethod
    def _record(cls, host, location: str | None, slot: str | None) -> dict:
        return {
            "name": host.name,
            "arch": host.arch,
            "product": host.product.name,
            "version": cls._ver_str(host.product.version),
            "addons": [a.name for a in host.addons],
            "location": location,
            "slot": slot,
        }

    def _gather(self) -> list[dict]:
        """Resolve refhosts and return the matched records (offline)."""
        refhosts = RefhostsFactory(self.config)
        a = self.args
        records: list[dict] = []
        if a.testplatform:
            attrs = Attributes.from_testplatform(a.testplatform)
            if a.pool:
                for host, slot in refhosts.search_pool(attrs, all_locations=True):
                    records.append(self._record(host, None, slot))
            else:
                for host, loc in refhosts.query(attributes=attrs, location=a.location):
                    records.append(self._record(host, loc, None))
        else:
            for host, loc in refhosts.query(
                name=a.name,
                arch=a.arch,
                product=a.product,
                version=a.version,
                addon=a.addon,
                location=a.location,
            ):
                slot = f"{host.product.name}-{self._ver_str(host.product.version)} {host.arch}"
                records.append(self._record(host, loc, slot if a.pool else None))
        return records

    def _probe_locks(self, records: list[dict]) -> None:
        """Connect to each matched host (best-effort, parallel) and record its
        live mtui-lock state under the ``lock`` key.
        """

        def probe(name: str) -> tuple[str, str]:
            target = Target(self.config, name, interactive=False)
            try:
                target.connect()
                holder = target.locked_by()
                return name, f"locked: {holder}" if holder else "free"
            except Exception:  # noqa: BLE001 - best-effort probe
                return name, "unreachable"
            finally:
                with contextlib.suppress(Exception):
                    target.close()

        with ContextExecutor() as executor:
            futures = [executor.submit(probe, r["name"]) for r in records]
            states = dict(f.result() for f in concurrent.futures.as_completed(futures))
        for r in records:
            r["lock"] = states.get(r["name"], "unknown")

    def __call__(self) -> None:
        """Executes the ``list_refhosts`` command."""
        records = self._gather()
        if self.args.free and records:
            self._probe_locks(records)

        if self.args.as_json:
            self.println(json.dumps(records, indent=2))
            return

        if not records:
            self.println("no refhosts match")
            return

        self._render_table(records)

    def _render_table(self, records: list[dict]) -> None:
        """Print an aligned human table (grouped by slot when --pool)."""
        verbose = self.args.verbose
        free = self.args.free

        def fmt(r: dict) -> str:
            prod = f"{r['product']} {r['version']}".strip()
            cols = [f"{r['name']:<34}", f"{prod:<22}", f"{r['arch']:<8}"]
            if not self.args.pool:
                cols.append(f"{r['location'] or '':<10}")
            if free:
                cols.append(f"{r.get('lock', ''):<22}")
            if verbose:
                cols.append(",".join(r["addons"]))
            return " ".join(cols).rstrip()

        if self.args.pool:
            by_slot: dict[str, list[dict]] = {}
            for r in records:
                by_slot.setdefault(r["slot"] or "?", []).append(r)
            for slot in sorted(by_slot):
                self.println(f"== {slot} ==")
                for r in by_slot[slot]:
                    self.println("  " + fmt(r))
        else:
            for r in records:
                self.println(fmt(r))

        self.println(f"\n{len(records)} refhost(s)")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-T", "--testplatform"),
                ("-n", "--name"),
                ("-a", "--arch"),
                ("-p", "--product"),
                ("--version",),
                ("--addon",),
                ("-l", "--location"),
                ("--pool",),
                ("--json",),
                ("--free",),
                ("-v", "--verbose"),
            ],
            line,
            text,
        )
