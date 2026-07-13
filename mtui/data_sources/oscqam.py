"""OSC review backend: native direct-OBS-API by default, legacy plugin fallback.

The ``OSC(config, rrid)`` seam keeps its 5 bool / never-raising methods. It
dispatches on ``[obs] backend``: ``native`` (default) calls the OBS API
directly via :mod:`mtui.data_sources.obs`, reading credentials from the
user's ``~/.oscrc``; ``plugin`` keeps the historical shell-out to the
external ``osc qam`` CLI as a transitional fallback.
"""

import xml.etree.ElementTree as ET
from collections.abc import Callable
from logging import getLogger
from shlex import join as shlex_join
from subprocess import DEVNULL, CalledProcessError, TimeoutExpired, run

import paramiko
import requests

from ..support.config import Config
from ..support.exceptions import ObsError
from ..types.enums import RequestKind
from ..types.rrid import RequestReviewID
from .obs import qam as obs_qam
from .obs.client import ObsClient
from .obs.oscrc import read_credentials

logger = getLogger("mtui.connector.oscqam")

API = "https://api.suse.de"


def _tail(text: str | None, limit: int = 2000) -> str:
    """Trim captured osc output to its last ``limit`` chars for logging.

    Args:
        text: Captured stdout/stderr, possibly ``None``.
        limit: Maximum number of characters to keep.

    Returns:
        The stripped text, truncated from the front (prefixed with an
        ellipsis) when longer than ``limit``; ``""`` when ``text`` is empty.

    """
    text = (text or "").strip()
    if len(text) <= limit:
        return text
    return "…" + text[-limit:]


class OSC:
    """A wrapper for interacting with the `osc qam` command-line tool."""

    def __init__(self, config: Config, rrid: RequestReviewID) -> None:
        """Initializes the OSC connector.

        Args:
            config: An instance of the application's Config class.
            rrid: A RequestReviewID object representing the target review.

        """
        self.config = config
        self.rrid = rrid

    def __operation(
        self,
        operation: str,
        groups: list[str],
        reason: str = "",
        message: str = "",
        comment: str = "",
    ) -> bool:
        """Constructs and executes `osc qam` commands safely.

        The command is built as an argv list and handed to
        ``subprocess.run`` without ``shell=True``, so each element is
        passed verbatim to ``execve`` and no shell ever interprets the
        tokens -- that alone is what prevents command injection. Because
        there is no shell, ``shlex.quote`` must NOT be applied to the
        message or comment: it would only add shell-escaping syntax that
        osc then receives as literal characters, corrupting the recorded
        text (e.g. ``does not build`` -> ``'does not build'``).
        ``shlex_join`` is used solely to render the readable debug log.

        Args:
            operation: The `qam` subcommand to perform (e.g., 'approve').
            groups: A list of group names to apply the operation to.
            reason: The reason for a rejection.
            message: The message to include with the operation.
            comment: A comment to add to the request.

        Returns:
            ``True`` when osc exited cleanly, ``False`` on any failure
            (non-zero exit, timeout, or osc not found). Failures are logged
            with osc's captured stderr so the caller learns *why*.

        """
        # Start with the base command components that are always present.
        base_cmd = ["osc", "-A", API, "qam", operation]

        # Dynamically build the list of group arguments (e.g., ["-G", "group1", "-G", "group2"]).
        group_args = []
        if groups:
            for g in groups:
                group_args.extend(["-G", g])

        # Conditionally add optional arguments to the command list.
        reason_args = ["-R", reason] if reason else []
        message_args = ["-M", message] if message else []

        # Add a specific workaround for 'PI' kinds which have a different RRID format
        # that oscqam does not expect by default.
        skip_args = (
            ["--skip-template"]
            if (
                self.rrid.kind in (RequestKind.PI, RequestKind.SLFO)
                and operation in ("assign", "approve", "reject")
            )
            else []
        )

        # must be converted to str -> shlex.join accepts only str or bytestr
        rrid_args = [str(self.rrid.review_id)]
        comment_args = [comment] if comment else []

        # Combine all parts into the final command list in the correct order.
        command: list[str] = (
            base_cmd
            + group_args
            + rrid_args
            + reason_args
            + skip_args
            + message_args
            + comment_args
        )

        logger.info("Performing '%s' operation on %s", operation, str(self.rrid))

        # For logging purposes, it's helpful to see the command as a single string.
        # shlex_join (or shlex.join) safely quotes each argument.
        logger.debug("Executing command: %s", shlex_join(command))

        try:
            # Never let osc inherit the server's stdin: under mtui-mcp that stdin is
            # the MCP stdio JSON-RPC pipe, so an interactive osc prompt (e.g. an
            # approve confirmation) would block reading it forever and deadlock the
            # single-threaded server. Feed EOF instead, cap the runtime so a stalled
            # osc can never wedge the session, and capture output so a failure can
            # report osc's actual reason instead of a bare exit code.
            proc = run(
                command,
                stdin=DEVNULL,
                capture_output=True,
                text=True,
                timeout=180,
                check=True,
            )

        except CalledProcessError as e:
            detail = _tail(e.stderr) or _tail(e.stdout) or f"exit code {e.returncode}"
            logger.error("'%s' operation failed: %s", operation, detail)
            if groups:
                # `osc qam <op> -G` first asks an interactive confirmation; with
                # stdin detached that prompt EOFs, so the -G path can only fail
                # headless. The group review reassigned to you on pickup is a plain
                # user review — approve/assign it without -G.
                logger.error(
                    "'%s' was called with -G/--group, whose osc confirmation prompt "
                    "cannot be answered without a terminal (e.g. under mtui-mcp). "
                    "Re-run '%s' without -G/--group to act on the review assigned "
                    "to you.",
                    operation,
                    operation,
                )
            logger.debug("Call stack trace:", stack_info=True)
            return False

        except TimeoutExpired:
            logger.error(
                "'%s' operation timed out after 180s; osc did not return "
                "(likely an interactive prompt with no input).",
                operation,
            )
            return False

        except FileNotFoundError:
            logger.error("'osc' command not found. Is it installed and in your PATH?")
            return False

        out = _tail(proc.stdout)
        if out:
            logger.info("'%s' succeeded: %s", operation, out)
        return True

    def _native(self, op: Callable[[ObsClient, str], None]) -> bool:
        """Run a native OBS operation, converting any failure into ``False``.

        Everything — reading oscrc, loading the key, building the session,
        the first authenticated call, XML parsing — happens here inside one
        try/except, because callers (apicall.py / approve.py) invoke the
        seam methods bare with no guard of their own.
        """
        try:
            credentials = read_credentials(
                self.config.obs_api_url, self.config.obs_conffile
            )
            client = ObsClient(self.config, credentials)
            op(client, credentials.user)
        except (
            ObsError,
            requests.RequestException,
            paramiko.SSHException,
            ET.ParseError,
        ) as e:
            logger.error("native OBS operation on %s failed: %s", self.rrid, e)
            return False
        return True

    def approve(self, group: list[str]) -> bool:
        """Approves a review request. Returns ``True`` on success."""
        if self.config.obs_backend == "native":
            return self._native(
                lambda c, u: obs_qam.approve(c, self.config, self.rrid, u, group or [])
            )
        return self.__operation("approve", group)

    def assign(self, group: list[str]) -> bool:
        """Assigns a review request. Returns ``True`` on success."""
        if self.config.obs_backend == "native":
            return self._native(
                lambda c, u: obs_qam.assign(c, self.config, self.rrid, u, group or [])
            )
        return self.__operation("assign", group)

    def unassign(self, group: list[str]) -> bool:
        """Unassigns a review request. Returns ``True`` on success."""
        if self.config.obs_backend == "native":
            return self._native(
                lambda c, u: obs_qam.unassign(c, self.config, self.rrid, u, group or [])
            )
        return self.__operation("unassign", group)

    def comment(self, comment: str) -> bool:
        """Adds a comment to a review request. Returns ``True`` on success."""
        if self.config.obs_backend == "native":
            return self._native(lambda c, _u: obs_qam.comment(c, self.rrid, comment))
        return self.__operation("comment", [], comment=comment)

    def reject(self, group: list[str], reason: str, message: str) -> bool:
        """Rejects a review request. Returns ``True`` on success."""
        if self.config.obs_backend == "native":
            return self._native(
                lambda c, u: obs_qam.reject(
                    c, self.config, self.rrid, u, group or [], reason, message
                )
            )
        return self.__operation("reject", group, reason=reason, message=message)
