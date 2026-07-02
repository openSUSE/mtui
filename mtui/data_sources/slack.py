"""A Slack Web API client for the request-review acknowledgement workflow.

mtui posts a review request into a Slack channel and then blocks on that
message: it streams any threaded replies back to the caller and watches the
message's reactions for a 👍 that marks the request acknowledged. The HTTP
shape mirrors the Gitea client (:mod:`mtui.data_sources.gitea`) and reuses the
shared timeout/TLS helpers from :mod:`mtui.support.http`.

Slack is unusual in that a failed call still returns HTTP 200 with an
``{"ok": false, "error": ...}`` body, so :meth:`SlackClient._call` checks both
``rsp.ok`` and the payload's ``ok`` flag before returning.

The base URL comes from ``[slack] base_url`` (defaults to
``https://slack.com/api``) and the bot token from ``[slack] token`` /
``SLACK_TOKEN``.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from logging import getLogger
from typing import Any
from urllib.parse import urlparse

import requests

from ..support.config import Config
from ..support.exceptions import FailedSlackCallError, MissingSlackTokenError
from ..support.http import (
    HTTP_TIMEOUT,
    VerifyPolicy,
    build_session,
    is_ssl_verification_error,
    resolve_verify,
    ssl_verification_hint,
)

logger = getLogger("mtui.connector.slack")

#: Reaction names that count as a positive acknowledgement of a request.
_ACK_REACTIONS = frozenset({"+1", "thumbsup"})

#: Consecutive failed poll cycles after which a review watch gives up as
#: unreachable. A watch spans hours, so a single transient failure (network
#: blip, sustained 429) must not abort it.
_MAX_POLL_FAILURES = 3


@dataclass(frozen=True)
class ReviewOutcome:
    """The result of blocking on a Slack review request (see :meth:`SlackClient.wait_for_ack`).

    - ``acked``: a 👍 reaction was seen on the request message.
    - ``reviewer``: best-effort display name of the acking user, or ``None``.
    - ``timed_out``: the wait ended on its deadline or a stop signal without
      an acknowledgement.
    - ``unreachable``: Slack could not be reached to resolve the outcome.
    """

    acked: bool
    reviewer: str | None
    timed_out: bool
    unreachable: bool


class SlackClient:
    """A Slack Web API client built on the shared HTTP helpers."""

    def __init__(self, config: Config) -> None:
        """Initialize the Slack client.

        Args:
            config: The application configuration object.

        Raises:
            MissingSlackTokenError: If the Slack bot token is not configured.

        """
        if not config.slack_token:
            raise MissingSlackTokenError("Slack token is empty, can't access API")

        self.headers = {
            "Authorization": f"Bearer {config.slack_token}",
            "Content-Type": "application/json;charset=utf-8",
        }
        self.base = config.slack_base_url.rstrip("/")

        # Resolve the TLS verification policy once (verify by default, let the
        # global ``[mtui] ssl_verify`` override) and reuse a single session
        # that silences the InsecureRequestWarning when verification is off.
        self._verify: VerifyPolicy = resolve_verify(True, config.ssl_verify)
        self._session = build_session(self._verify)

        # ``auth.test`` is stable for the life of the token, so cache it.
        self._bot_user_id: str | None = None

    def _call(
        self,
        http_method: str,
        api_method: str,
        *,
        json: dict[str, Any] | None = None,
        params: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Make a request to a Slack Web API method and return its payload.

        Slack signals failure two ways: a non-2xx HTTP status, or an HTTP 200
        with an ``{"ok": false, "error": ...}`` body. Both are treated as
        failures here. A 429 with a ``Retry-After`` header is honoured with a
        single wait-and-retry before giving up.

        Args:
            http_method: The HTTP verb (``GET``/``POST``).
            api_method: The Slack Web API method name (e.g. ``chat.postMessage``).
            json: JSON body for the request.
            params: URL query parameters for the request.

        Returns:
            The decoded Slack payload dict (guaranteed ``ok: true``).

        Raises:
            FailedSlackCallError: On any transport failure, non-2xx status, or
                an ``ok: false`` Slack payload.

        """
        url = f"{self.base}/{api_method}"
        # Honour rate limiting with a single wait-and-retry: one 429 is retried,
        # a second consecutive 429 gives up (bounded so a sustained rate limit
        # can never trap the caller — the review wait must keep reaching its own
        # deadline / cancellation).
        rate_limited = False
        while True:
            try:
                logger.debug("Requesting %s on %s", http_method, url)
                rsp = self._session.request(
                    http_method,
                    url,
                    headers=self.headers,
                    params=params,
                    json=json,
                    timeout=HTTP_TIMEOUT,
                )
            except requests.exceptions.RequestException as e:
                if is_ssl_verification_error(e):
                    # Surface the actionable remedy at ERROR instead of a
                    # multi-frame traceback (mirrors the Gitea client).
                    logger.error(ssl_verification_hint(urlparse(url).hostname))
                    logger.debug("Slack TLS error detail: %s", e)
                else:
                    logger.exception("API call to Slack failed: %s", e)
                raise FailedSlackCallError(f"{http_method} - {url}") from e

            if rsp.status_code == 429:
                if rate_limited:
                    raise FailedSlackCallError(
                        f"{http_method} - {url} rate-limited (429) after retry"
                    )
                rate_limited = True
                # ``Retry-After`` may be delta-seconds or an HTTP-date; only the
                # former is actionable here, fall back to 1s otherwise.
                try:
                    retry_after = int(rsp.headers.get("Retry-After", "1"))
                except ValueError:
                    retry_after = 1
                logger.warning(
                    "Slack rate-limited %s; retrying in %ss", url, retry_after
                )
                time.sleep(retry_after)
                continue

            if not rsp.ok:
                logger.warning(
                    "API call to %s failed with status code: %s", url, rsp.status_code
                )
                raise FailedSlackCallError(
                    f"{http_method} - {url} returned status {rsp.status_code}"
                )

            try:
                payload = rsp.json()
            except requests.exceptions.JSONDecodeError as e:
                raise FailedSlackCallError(f"{http_method} - {url}") from e

            # Slack returns HTTP 200 even for logical failures; the body's
            # ``ok`` flag is authoritative.
            if not payload.get("ok"):
                error = payload.get("error", "unknown")
                logger.warning("Slack %s returned error: %s", api_method, error)
                raise FailedSlackCallError(error)

            return payload

    def bot_user_id(self) -> str:
        """Return the bot's own user id (``auth.test``), cached on the instance."""
        if self._bot_user_id is None:
            payload = self._call("GET", "auth.test")
            self._bot_user_id = payload["user_id"]
        return self._bot_user_id

    def chat_postMessage(  # noqa: N802 - mirrors the Slack API method name
        self, channel: str, text: str, thread_ts: str | None = None
    ) -> str:
        """Post a message to a channel (optionally in a thread) and return its ``ts``.

        Args:
            channel: The channel id to post into.
            text: The message text.
            thread_ts: The parent message ``ts`` to reply under, or ``None`` to
                post a new top-level message.

        Returns:
            The posted message's ``ts`` (its channel-unique timestamp id).

        """
        body: dict[str, Any] = {"channel": channel, "text": text}
        if thread_ts is not None:
            body["thread_ts"] = thread_ts
        payload = self._call("POST", "chat.postMessage", json=body)
        return payload["ts"]

    def conversations_replies(  # noqa: N802 - mirrors the Slack API method name
        self, channel: str, ts: str
    ) -> list[dict[str, Any]]:
        """Return a thread's messages, oldest first (index 0 is the parent).

        Args:
            channel: The channel id the thread lives in.
            ts: The parent message ``ts``.

        Returns:
            The list of message dicts; the first is the parent/request itself.

        """
        payload = self._call(
            "GET", "conversations.replies", params={"channel": channel, "ts": ts}
        )
        return payload.get("messages", [])

    def reactions_get(  # noqa: N802 - mirrors the Slack API method name
        self, channel: str, ts: str
    ) -> list[dict[str, Any]]:
        """Return the reactions on a message, or ``[]`` if there are none.

        Args:
            channel: The channel id the message lives in.
            ts: The message ``ts``.

        Returns:
            The ``message.reactions`` list (each with ``name``/``users``), or
            an empty list when the message carries no reactions.

        """
        payload = self._call(
            "GET", "reactions.get", params={"channel": channel, "timestamp": ts}
        )
        return payload.get("message", {}).get("reactions", [])

    def users_info(  # noqa: N802 - mirrors the Slack API method name
        self, user: str
    ) -> str:
        """Return a human-friendly name for a user id.

        Prefers the profile display name, then the profile real name, then the
        bare ``name`` handle.

        Args:
            user: The Slack user id.

        Returns:
            The best available display name for the user.

        """
        payload = self._call("GET", "users.info", params={"user": user})
        info = payload.get("user", {})
        profile = info.get("profile", {})
        return (
            profile.get("display_name")
            or profile.get("real_name")
            or info.get("name", user)
        )

    def wait_for_ack(
        self,
        channel: str,
        ts: str,
        *,
        on_reply: Callable[[str], None],
        should_stop: Callable[[], bool],
        interval: float,
        timeout: float,
        cancel_event: threading.Event | None = None,
    ) -> ReviewOutcome:
        """Block until the request message is 👍-acked, or the wait ends.

        Polls the thread and reactions of the request message every
        ``interval`` seconds up to ``timeout`` seconds. Each cycle streams any
        new threaded replies to ``on_reply`` and checks the reactions for a
        positive acknowledgement (see :data:`_ACK_REACTIONS`). The inter-poll
        sleep is stepped in ~0.1s slices via a :class:`threading.Event` so a
        stop signal or cancellation takes effect promptly instead of after up
        to ``interval`` seconds (cloned from
        :meth:`mtui.data_sources.teregen.TeReGen.wait_for_template`).

        Args:
            channel: The channel id the request lives in.
            ts: The request message ``ts``.
            on_reply: Called with the text of each new threaded reply, in order.
            should_stop: Polled before every sleep; returning ``True`` ends the
                wait with ``timed_out=True``.
            interval: Seconds between polls.
            timeout: Total seconds to wait before giving up.
            cancel_event: An optional event that, when set, ends the wait like
                ``should_stop`` (also used as the interruptible sleeper).

        Returns:
            A :class:`ReviewOutcome`: ``acked=True`` with a best-effort
            ``reviewer`` on acknowledgement, otherwise ``timed_out=True`` (or
            ``unreachable=True`` after :data:`_MAX_POLL_FAILURES` consecutive
            failed poll cycles; a single transient failure is retried).

        """
        deadline = time.monotonic() + timeout
        sleeper = cancel_event or threading.Event()
        # Skip the parent (index 0) so replies begin at 1; the last-seen index
        # advances as we fire ``on_reply`` for each new reply.
        last_seen = 1
        # A review watch spans hours; one transient blip (a flaky network, two
        # consecutive 429s) must not abort it. Only give up as unreachable
        # after several *consecutive* failed poll cycles.
        consecutive_failures = 0

        while True:
            try:
                messages = self.conversations_replies(channel, ts)
                for message in messages[last_seen:]:
                    on_reply(message.get("text", ""))
                    last_seen += 1

                reactions = self.reactions_get(channel, ts)
                if reviewer := self._acking_reviewer(reactions):
                    return ReviewOutcome(
                        acked=True,
                        reviewer=reviewer,
                        timed_out=False,
                        unreachable=False,
                    )
                consecutive_failures = 0
            except FailedSlackCallError as e:
                consecutive_failures += 1
                logger.warning(
                    "Slack poll for %s/%s failed (%s/%s consecutive): %s",
                    channel,
                    ts,
                    consecutive_failures,
                    _MAX_POLL_FAILURES,
                    e,
                )
                if consecutive_failures >= _MAX_POLL_FAILURES:
                    return ReviewOutcome(
                        acked=False, reviewer=None, timed_out=False, unreachable=True
                    )

            # Check the stop conditions before sleeping so we exit promptly.
            stopped = should_stop() or (
                cancel_event is not None and cancel_event.is_set()
            )
            if stopped or time.monotonic() >= deadline:
                return ReviewOutcome(
                    acked=False, reviewer=None, timed_out=True, unreachable=False
                )

            # Interruptible sleep: stepped in ~0.1s slices so a stop signal or
            # a set ``cancel_event`` takes effect within a tick.
            step = 0.1
            waited = 0.0
            while waited < interval and not should_stop():
                if cancel_event is not None and cancel_event.is_set():
                    break
                sleeper.wait(min(step, interval - waited))
                waited += step

    def _acking_reviewer(self, reactions: list[dict[str, Any]]) -> str | None:
        """Resolve the reviewer name from an ack reaction, or ``None``.

        Scans the reactions for a positive-acknowledgement name and, if found,
        resolves the first non-bot user in its ``users`` list to a display
        name.

        BEST-EFFORT reviewer: Slack does not guarantee the ordering of a
        reaction's ``users`` list, so the "first" non-bot user is only an
        approximation of who acked first.
        """
        bot_id = self.bot_user_id()
        for reaction in reactions:
            if reaction.get("name") not in _ACK_REACTIONS:
                continue
            for user in reaction.get("users", []):
                if user != bot_id:
                    try:
                        return self.users_info(user)
                    except FailedSlackCallError as e:
                        # The ack itself is authoritative; the display name is
                        # only best-effort garnish. A users.info failure (most
                        # commonly a bot token without the ``users:read``
                        # scope) must not turn a visible 👍 into a dead watch —
                        # fall back to the raw member id.
                        logger.warning(
                            "Could not resolve Slack user %s to a name "
                            "(missing 'users:read' scope?): %s -- recording "
                            "the raw id",
                            user,
                            e,
                        )
                        return user
        return None
