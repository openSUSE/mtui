"""The `downgrade` command."""

from logging import getLogger
from traceback import format_exc

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..support.exceptions import UpdateError
from ..support.messages import NoRefhostsDefinedError
from ..support.misc import requires_update
from ..types.rpmver import RPMVersion
from . import Command

logger = getLogger("mtui.command.downgrade")


class Downgrade(Command):
    """Downgrades all related packages to the last released version.

    Warning:
        This command cannot work for new packages.

    """

    command = "downgrade"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `downgrade` command."""
        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        logger.info("Downgrading")

        try:
            self.metadata.perform_downgrade(targets)
        except KeyboardInterrupt:
            logger.info("downgrade process canceled")
            return
        except UpdateError as e:
            # The workflow aborted mid-rollback (the version probe or a
            # downgrade command died): some or all packages are still at the
            # update version. Say so explicitly -- a bare "failed" reads like
            # nothing happened, when the dangerous outcome is a half-rollback.
            logger.error("downgrade failed: %s", e)
            logger.error(
                "packages may still be at the update version; "
                "verify with 'rpm -q' and re-run downgrade"
            )
            logger.error("downgrade not completed")
            # Headless callers (mtui-mcp) read structured success, not logs:
            # re-raise so the tool call fails. An Exception, never SystemExit,
            # so the per-template fan-out handling stays intact.
            if not getattr(self.prompt, "interactive", True):
                raise
            return
        except Exception:
            logger.critical("failed to downgrade target systems")
            logger.debug(format_exc())
            return

        # Verify the rollback actually happened. A package still at (or above)
        # the update's shipped version did not downgrade -- report it per host
        # at ERROR, naming the packages, so a caller who doesn't re-verify with
        # rpm -q can't mistake a half-rollback for success. (The comparison
        # against ``required`` also stays quiet when the update was never
        # installed: those packages sit below ``required`` already.)
        not_downgraded: dict[str, list[str]] = {}
        for target in targets.values():
            target.query_versions()
            for name, package in target.packages.items():
                package.before = package.after
                package.after = package.current
                required, current = package.required, package.current
                if (
                    isinstance(required, RPMVersion)
                    and isinstance(current, RPMVersion)
                    and current >= required
                ):
                    not_downgraded.setdefault(target.hostname, []).append(
                        f"{name} (at {current}, update ships {required})"
                    )

        if not_downgraded:
            for hostname, names in not_downgraded.items():
                logger.error(
                    "%s: still at or above the update's shipped version "
                    "after downgrade: %s",
                    hostname,
                    ", ".join(names),
                )
            logger.error(
                "downgrade not completed; verify with 'rpm -q'. New packages "
                "(no released version to go back to) and multiversion "
                "packages (e.g. the kernel) always appear here; re-running "
                "downgrade will not clear them"
            )
            if not getattr(self.prompt, "interactive", True):
                raise UpdateError("downgrade not completed")
        else:
            logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), *template_completion(state)],
            line,
            text,
            state["hosts"].names(),
        )
