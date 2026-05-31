"""Per-target zypper repository manager.

This module is the home of the two repository-shape methods that used
to live directly on :class:`Target`:

* ``set(operation, testreport)`` — the one-line forward into
  ``testreport.set_repo(self, operation)``; kept on the collaborator
  so callers reach for ``target.repo_manager.set(...)`` instead of
  ``target.set_repo(...)``.
* ``run_zypper(cmd, repos, rrid)`` — fans the zypper ar/rr add/remove
  loop out across the target's flattened system, refreshes at the end.

The unknown-cmd safeguard (``unlock(force=True)`` followed by a bare
``ValueError``) is preserved byte-for-byte.
"""

from logging import getLogger
from typing import TYPE_CHECKING, final

if TYPE_CHECKING:
    from .target import Target

logger = getLogger("mtui.target.repo_manager")


@final
class RepoManager:
    """Adapter that owns the per-target zypper-repo lifecycle."""

    def __init__(self, target: "Target") -> None:
        """Bind the manager to ``target``."""
        self.target = target

    def set(self, operation: str, testreport) -> None:
        """Ask ``testreport`` to add or remove repos on the bound target.

        Mirrors the one-line forward that used to live as
        ``Target.set_repo``.
        """
        t = self.target
        logger.debug("%s: changing %s repos", t.hostname, operation)
        testreport.set_repo(t, operation)

    def run_zypper(self, cmd, repos, rrid) -> None:
        """Run a fan-out ``zypper`` command across the target's repos.

        Iterates the ``repos`` mapping filtered by what the target's
        flattened system actually carries; for each product/repo pair,
        ``cmd`` is appended into either ``zypper ar <alias> <url>
        <alias>`` (when ``"ar"`` is in ``cmd``) or ``zypper rr <url>``
        (when ``"rr"`` is in ``cmd``). Unknown sub-commands force-unlock
        and raise ``ValueError`` — pin that safeguard explicitly with
        the unit test.

        Always finishes with ``zypper -n ref`` so subsequent operations
        see the freshly-(un)registered repos.
        """
        t = self.target
        # ur - generator returning tuples of (product, repo_url)
        ur = ((x, y) for x, y in repos.items() if x in t.system.flatten())

        def name(product, rrid) -> str:
            return f"issue-{product.name}:{product.version}:p={rrid.maintenance_id}:{rrid.review_id}"

        for x, y in ur:
            if "ar" in cmd:
                logger.info("Adding repo %s on %s", y, t.hostname)
                t.run(f"zypper {cmd} {name(x, rrid)} {y} {name(x, rrid)}")
            elif "rr" in cmd:
                logger.info("Removing repo %s on %s", y, t.hostname)
                t.run(f"zypper {cmd} {y}")
            else:
                t.unlock(force=True)
                raise ValueError

        t.run("zypper -n ref")
