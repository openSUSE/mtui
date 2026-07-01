"""Tests for the mtui Slack connector."""

import threading
from json import loads
from unittest.mock import patch

import pytest
import requests
import responses

from mtui.data_sources.slack import ReviewOutcome, SlackClient, is_ack_reaction
from mtui.support.exceptions import FailedSlackCallError, MissingSlackTokenError


def _api(config, method: str) -> str:
    """Full URL of a Slack API method against the mocked base."""
    return f"{config.slack_base_url.rstrip('/')}/{method}"


def _body(call) -> dict:
    """Decode the JSON request body of a captured ``responses`` call."""
    raw = call.request.body
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8")
    return loads(raw)


def _query(call) -> dict:
    """Parsed query-string params of a captured ``responses`` call."""
    from urllib.parse import parse_qs, urlparse

    return {k: v[0] for k, v in parse_qs(urlparse(call.request.url).query).items()}


# --- Construction ---


class TestSlackClientInit:
    def test_missing_token_raises(self, mock_config):
        """An empty token fails fast with MissingSlackTokenError."""
        mock_config.slack_token = ""
        with pytest.raises(MissingSlackTokenError):
            SlackClient(mock_config)  # type: ignore[arg-type]

    def test_init_sets_auth_header_and_base(self, mock_config):
        """The bearer header and normalised base URL are set up."""
        mock_config.slack_base_url = "https://slack.test/api/"
        client = SlackClient(mock_config)  # type: ignore[arg-type]
        assert client.headers["Authorization"] == f"Bearer {mock_config.slack_token}"
        assert "application/json" in client.headers["Content-Type"]
        # Trailing slash stripped so ``base/method`` joins cleanly.
        assert client.base == "https://slack.test/api"


# --- Ack-reaction matching ---


class TestIsAckReaction:
    @pytest.mark.parametrize(
        "name", ["+1", "thumbsup", "+1::skin-tone-3", "thumbsup::skin-tone-6"]
    )
    def test_ack_names_match(self, name):
        """Base and skin-toned 👍 names all count as an acknowledgement."""
        assert is_ack_reaction(name) is True

    @pytest.mark.parametrize("name", ["eyes", "-1", "-1::skin-tone-3", ""])
    def test_non_ack_names_do_not_match(self, name):
        """Other reactions (even skin-toned ones) never count."""
        assert is_ack_reaction(name) is False


# --- Individual API methods ---


class TestSlackClientCalls:
    @pytest.fixture
    def client(self, mock_config):
        return SlackClient(mock_config)  # type: ignore[arg-type]

    @responses.activate
    def test_chat_post_message_returns_channel_and_ts(self, client, mock_config):
        """chat_postMessage returns (channel, ts) and posts the right channel/text."""
        responses.add(
            responses.POST,
            _api(mock_config, "chat.postMessage"),
            json={"ok": True, "channel": "C123", "ts": "1700000000.000100"},
            status=200,
        )

        channel, ts = client.chat_postMessage("C123", "please review")

        assert channel == "C123"
        assert ts == "1700000000.000100"
        body = _body(responses.calls[0])
        assert body["channel"] == "C123"
        assert body["text"] == "please review"
        # No thread_ts for a top-level post.
        assert "thread_ts" not in body

    @responses.activate
    def test_chat_post_message_returns_canonical_channel_id(self, client, mock_config):
        """Posting by channel name yields the canonical id from the response.

        conversations.replies/reactions.get accept only channel ids, so the
        id Slack resolved the name to is the one callers must persist -- not
        the configured name that was posted with.
        """
        responses.add(
            responses.POST,
            _api(mock_config, "chat.postMessage"),
            json={"ok": True, "channel": "C999", "ts": "1700000000.000300"},
            status=200,
        )

        channel, ts = client.chat_postMessage("#reviews", "please review")

        assert channel == "C999"
        assert ts == "1700000000.000300"
        # The request itself still carried the name Slack was asked to resolve.
        assert _body(responses.calls[0])["channel"] == "#reviews"

    @responses.activate
    def test_chat_post_message_threads_reply(self, client, mock_config):
        """A thread_ts is forwarded when replying in a thread."""
        responses.add(
            responses.POST,
            _api(mock_config, "chat.postMessage"),
            json={"ok": True, "channel": "C123", "ts": "1700000000.000200"},
            status=200,
        )

        client.chat_postMessage("C123", "in thread", thread_ts="1700000000.000100")

        assert _body(responses.calls[0])["thread_ts"] == "1700000000.000100"

    @responses.activate
    def test_ok_false_raises_failed_call(self, client, mock_config):
        """An HTTP 200 with ok:false is a failure carrying the Slack error."""
        responses.add(
            responses.POST,
            _api(mock_config, "chat.postMessage"),
            json={"ok": False, "error": "channel_not_found"},
            status=200,
        )

        with pytest.raises(FailedSlackCallError, match="channel_not_found"):
            client.chat_postMessage("C123", "hi")

    @responses.activate
    def test_non_2xx_raises_failed_call(self, client, mock_config):
        """A non-2xx HTTP status is a failure too."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"error": "server_error"},
            status=500,
        )

        with pytest.raises(FailedSlackCallError):
            client.bot_user_id()

    @responses.activate
    def test_rate_limit_is_retried(self, client, mock_config):
        """A 429 with Retry-After is honoured with one wait-and-retry."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"error": "ratelimited"},
            status=429,
            headers={"Retry-After": "0"},
        )
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"ok": True, "user_id": "UBOT"},
            status=200,
        )

        assert client.bot_user_id() == "UBOT"
        assert len(responses.calls) == 2

    @responses.activate
    def test_rate_limit_gives_up_after_one_retry(self, client, mock_config):
        """A second consecutive 429 is bounded: it raises instead of looping."""
        for _ in range(2):
            responses.add(
                responses.GET,
                _api(mock_config, "auth.test"),
                json={"error": "ratelimited"},
                status=429,
                headers={"Retry-After": "0"},
            )

        with pytest.raises(FailedSlackCallError):
            client.bot_user_id()
        assert len(responses.calls) == 2  # original + one retry, then give up

    @responses.activate
    def test_rate_limit_non_numeric_retry_after(self, client, mock_config):
        """A non-delta-seconds Retry-After (HTTP-date) falls back, not raises."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"error": "ratelimited"},
            status=429,
            headers={"Retry-After": "Wed, 21 Oct 2099 07:28:00 GMT"},
        )
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"ok": True, "user_id": "UBOT"},
            status=200,
        )

        with patch("mtui.data_sources.slack.time.sleep"):
            assert client.bot_user_id() == "UBOT"

    @responses.activate
    def test_rate_limit_retry_after_is_capped_and_sliced(self, client, mock_config):
        """A huge server-sent Retry-After is capped at 60s and slept in ~1s slices."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"error": "ratelimited"},
            status=429,
            headers={"Retry-After": "3600"},
        )
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"ok": True, "user_id": "UBOT"},
            status=200,
        )

        with patch("mtui.data_sources.slack.time.sleep") as sleep:
            assert client.bot_user_id() == "UBOT"

        slices = [call.args[0] for call in sleep.call_args_list]
        assert sum(slices) == 60  # capped, not the server's 3600
        assert max(slices) <= 1.0  # sliced so the cancel check can interject

    @responses.activate
    def test_conversations_replies_returns_messages(self, client, mock_config):
        """conversations_replies returns the thread messages (parent first)."""
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [
                    {"text": "the request"},
                    {"text": "a reply"},
                ],
            },
            status=200,
        )

        messages = client.conversations_replies("C123", "111.222")

        assert [m["text"] for m in messages] == ["the request", "a reply"]
        params = _query(responses.calls[0])
        assert params["channel"] == "C123"
        assert params["ts"] == "111.222"

    @responses.activate
    def test_conversations_replies_follows_pagination(self, client, mock_config):
        """A multi-page thread is followed via next_cursor until exhausted."""
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [
                    {"ts": "111.222", "text": "the request"},
                    {"ts": "111.333", "text": "first reply"},
                ],
                "has_more": True,
                "response_metadata": {"next_cursor": "cur1"},
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [{"ts": "111.444", "text": "second reply"}],
                "has_more": False,
            },
            status=200,
        )

        messages = client.conversations_replies("C123", "111.222")

        assert [m["text"] for m in messages] == [
            "the request",
            "first reply",
            "second reply",
        ]
        # The first call carries no cursor; the second passes the returned one.
        assert "cursor" not in _query(responses.calls[0])
        assert _query(responses.calls[1])["cursor"] == "cur1"

    @responses.activate
    def test_conversations_replies_stops_on_empty_cursor(self, client, mock_config):
        """has_more without a usable next_cursor stops instead of re-fetching."""
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [{"ts": "111.222", "text": "the request"}],
                "has_more": True,
                "response_metadata": {"next_cursor": ""},
            },
            status=200,
        )

        messages = client.conversations_replies("C123", "111.222")

        assert len(messages) == 1
        assert len(responses.calls) == 1

    @responses.activate
    def test_conversations_replies_pagination_is_bounded(self, client, mock_config):
        """A never-ending cursor stops at the page cap instead of looping."""
        # responses replays the last mock forever: every page claims more.
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [{"ts": "1", "text": "x"}],
                "has_more": True,
                "response_metadata": {"next_cursor": "again"},
            },
            status=200,
        )

        messages = client.conversations_replies("C123", "111.222")

        # Bounded at the 10-page cap (one message per mocked page).
        assert len(responses.calls) == 10
        assert len(messages) == 10

    @responses.activate
    def test_reactions_get_returns_reactions(self, client, mock_config):
        """reactions_get returns the message's reaction list."""
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {
                    "reactions": [{"name": "thumbsup", "users": ["U1"], "count": 1}]
                },
            },
            status=200,
        )

        reactions = client.reactions_get("C123", "111.222")

        assert reactions == [{"name": "thumbsup", "users": ["U1"], "count": 1}]
        assert _query(responses.calls[0])["timestamp"] == "111.222"

    @responses.activate
    def test_reactions_get_empty_when_no_reactions(self, client, mock_config):
        """reactions_get returns [] for a message with no reactions."""
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {}},
            status=200,
        )

        assert client.reactions_get("C123", "111.222") == []

    @responses.activate
    def test_transport_error_raises_failed_call(self, client, mock_config):
        """A transport-level failure surfaces as FailedSlackCallError."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            body=requests.exceptions.ConnectionError("boom"),
        )

        with pytest.raises(FailedSlackCallError):
            client.bot_user_id()

    @responses.activate
    def test_ssl_verification_error_raises_failed_call(self, client, mock_config):
        """A TLS verification failure is wrapped, not propagated raw."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            body=requests.exceptions.SSLError("CERTIFICATE_VERIFY_FAILED"),
        )

        with pytest.raises(FailedSlackCallError):
            client.bot_user_id()

    @responses.activate
    def test_non_json_body_raises_failed_call(self, client, mock_config):
        """An HTTP 200 with a non-JSON body (e.g. a proxy error page) fails."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            body="<html>gateway error</html>",
            status=200,
            content_type="text/html",
        )

        with pytest.raises(FailedSlackCallError):
            client.bot_user_id()

    @responses.activate
    def test_users_info_prefers_display_name(self, client, mock_config):
        """users_info returns the profile display name when present."""
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={
                "ok": True,
                "user": {
                    "name": "handle",
                    "profile": {"display_name": "Alice A", "real_name": "Alice Adams"},
                },
            },
            status=200,
        )

        assert client.users_info("U1") == "Alice A"
        assert _query(responses.calls[0])["user"] == "U1"

    @responses.activate
    def test_users_info_falls_back_to_real_name_then_name(self, client, mock_config):
        """users_info falls back to real_name, then the bare handle."""
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={
                "ok": True,
                "user": {"name": "handle", "profile": {"real_name": "Real Name"}},
            },
            status=200,
        )
        assert client.users_info("U1") == "Real Name"

        responses.replace(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"name": "handle", "profile": {}}},
            status=200,
        )
        assert client.users_info("U1") == "handle"

    @responses.activate
    def test_bot_user_id_is_cached(self, client, mock_config):
        """bot_user_id hits auth.test once and caches the result."""
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"ok": True, "user_id": "UBOT"},
            status=200,
        )

        assert client.bot_user_id() == "UBOT"
        assert client.bot_user_id() == "UBOT"
        assert len(responses.calls) == 1


# --- The blocking watch loop ---


class TestWaitForAck:
    @pytest.fixture
    def client(self, mock_config):
        return SlackClient(mock_config)  # type: ignore[arg-type]

    def _add_auth_test(self, mock_config, user_id="UBOT"):
        responses.add(
            responses.GET,
            _api(mock_config, "auth.test"),
            json={"ok": True, "user_id": user_id},
            status=200,
        )

    @responses.activate
    def test_streams_replies_then_acks_with_reviewer(self, client, mock_config):
        """An ordered no-reaction-then-👍 sequence acks and names the reviewer.

        The first cycle sees a new reply (streamed to ``on_reply``) and no
        reaction; the second cycle sees the 👍 and resolves the reviewer.
        """
        self._add_auth_test(mock_config)

        # conversations.replies: cycle 1 has one new reply, cycle 2 unchanged.
        for _ in range(2):
            responses.add(
                responses.GET,
                _api(mock_config, "conversations.replies"),
                json={
                    "ok": True,
                    "messages": [
                        {"ts": "111.222", "text": "the request"},
                        {"ts": "111.333", "text": "looking now"},
                    ],
                },
                status=200,
            )
        # reactions.get: cycle 1 none, cycle 2 a thumbsup by a human.
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {"reactions": []}},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {"reactions": [{"name": "+1", "users": ["U1"]}]},
            },
            status=200,
        )
        # users.info for the reviewer.
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Reviewer R"}}},
            status=200,
        )

        replies: list[str] = []
        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=replies.append,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome == ReviewOutcome(
            acked=True, reviewer="Reviewer R", timed_out=False, unreachable=False
        )
        # Only the new reply past the parent was streamed.
        assert replies == ["looking now"]

    @responses.activate
    def test_bot_self_reaction_is_filtered(self, client, mock_config):
        """A 👍 from the bot itself does not count; the human behind it does."""
        self._add_auth_test(mock_config, user_id="UBOT")
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        # Bot's own id listed first, a human second -> the human is chosen.
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {
                    "reactions": [{"name": "thumbsup", "users": ["UBOT", "U2"]}]
                },
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Human H"}}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is True
        assert outcome.reviewer == "Human H"
        # users.info was queried for the human, not the bot.
        assert _query(responses.calls[-1])["user"] == "U2"

    @responses.activate
    def test_bot_only_reaction_does_not_ack(self, client, mock_config):
        """When only the bot has reacted, no reviewer is found and we time out."""
        self._add_auth_test(mock_config, user_id="UBOT")
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {"reactions": [{"name": "+1", "users": ["UBOT"]}]},
            },
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: True,  # stop after the first cycle
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is False
        assert outcome.reviewer is None
        assert outcome.timed_out is True

    @responses.activate
    def test_should_stop_returns_not_acked(self, client, mock_config):
        """A should_stop signal ends the wait with acked=False/timed_out=True."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {"reactions": []}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: True,
            interval=0.01,
            timeout=5,
        )

        assert outcome == ReviewOutcome(
            acked=False, reviewer=None, timed_out=True, unreachable=False
        )

    @responses.activate
    def test_zero_timeout_returns_not_acked(self, client, mock_config):
        """A non-positive timeout gives up after the first (non-acking) cycle."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {"reactions": []}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=0,
        )

        assert outcome.acked is False
        assert outcome.timed_out is True

    @responses.activate
    def test_cancel_event_ends_wait(self, client, mock_config):
        """A pre-set cancel_event ends the wait like should_stop."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {"reactions": []}},
            status=200,
        )

        cancel = threading.Event()
        cancel.set()
        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
            cancel_event=cancel,
        )

        assert outcome.acked is False
        assert outcome.timed_out is True

    @responses.activate
    def test_slack_failure_marks_unreachable(self, client, mock_config):
        """Repeated consecutive poll failures end the wait as unreachable."""
        self._add_auth_test(mock_config)
        # responses replays the last registered mock, so every cycle fails.
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": False, "error": "channel_not_found"},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is False
        assert outcome.unreachable is True
        # It took the full failure budget, not a single blip, to give up.
        failed_polls = [
            c
            for c in responses.calls
            if "conversations.replies" in (c.request.url or "")
        ]
        assert len(failed_polls) == 3

    @responses.activate
    def test_transient_poll_failure_is_retried(self, client, mock_config):
        """One failed poll cycle is retried instead of aborting the watch."""
        self._add_auth_test(mock_config)
        # Cycle 1: replies fails (transient); cycle 2: ok with an ack present.
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": False, "error": "ratelimited"},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {"reactions": [{"name": "+1", "users": ["U1"]}]},
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Reviewer R"}}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is True
        assert outcome.reviewer == "Reviewer R"
        assert outcome.unreachable is False

    @responses.activate
    def test_users_info_failure_still_acks_with_raw_id(self, client, mock_config):
        """A users.info failure (missing users:read scope) must not lose the ack."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {"reactions": [{"name": "+1", "users": ["U1"]}]},
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": False, "error": "missing_scope"},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        # The ack survives; the reviewer falls back to the raw member id.
        assert outcome.acked is True
        assert outcome.reviewer == "U1"

    @responses.activate
    def test_non_ack_reactions_are_skipped(self, client, mock_config):
        """Reactions outside the ack set don't ack; a later 👍 still does."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"ok": True, "messages": [{"text": "the request"}]},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {
                    "reactions": [
                        {"name": "eyes", "users": ["U9"]},
                        {"name": "+1", "users": ["U1"]},
                    ]
                },
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Reviewer R"}}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is True
        assert outcome.reviewer == "Reviewer R"

    @responses.activate
    def test_cancel_event_set_mid_sleep_breaks_promptly(self, client, mock_config):
        """A cancel_event set during the inter-poll sleep ends the wait quickly.

        The event is set from inside the sleep loop's ``should_stop`` probe, so
        the stepped sleep must break out immediately instead of waiting out the
        (deliberately long) interval; the next cycle's stop check then ends the
        wait. The test would hang for ``interval`` seconds if the break path
        were broken.
        """
        self._add_auth_test(mock_config)
        # Two poll cycles: the first sees nothing, the event is set during the
        # following sleep, the second cycle's stop check returns timed_out.
        for _ in range(2):
            responses.add(
                responses.GET,
                _api(mock_config, "conversations.replies"),
                json={"ok": True, "messages": [{"text": "the request"}]},
                status=200,
            )
            responses.add(
                responses.GET,
                _api(mock_config, "reactions.get"),
                json={"ok": True, "message": {"reactions": []}},
                status=200,
            )

        cancel = threading.Event()
        probes = 0

        def should_stop() -> bool:
            nonlocal probes
            probes += 1
            if probes >= 2:  # the sleep loop's probe, after the stop check
                cancel.set()
            return False

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=should_stop,
            interval=30,
            timeout=60,
            cancel_event=cancel,
        )

        assert outcome.acked is False
        assert outcome.timed_out is True
        assert outcome.unreachable is False

    @responses.activate
    def test_skin_toned_thumbsup_acks(self, client, mock_config):
        """A 👍 with a skin-tone suffix ("+1::skin-tone-3") still acks."""
        self._add_auth_test(mock_config)
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [{"ts": "111.222", "text": "the request"}],
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {
                    "reactions": [{"name": "+1::skin-tone-3", "users": ["U1"]}]
                },
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Reviewer R"}}},
            status=200,
        )

        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=lambda _t: None,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is True
        assert outcome.reviewer == "Reviewer R"

    @responses.activate
    def test_deleted_reply_does_not_swallow_next_reply(self, client, mock_config):
        """Replies are tracked by ts, so a deletion can't hide the next one.

        Cycle 1 sees two replies; before cycle 2 the first is deleted and a
        third arrives, keeping the list length constant. Index-based tracking
        would forward nothing on cycle 2 -- ts tracking forwards the new one
        (and re-forwards nothing).
        """
        self._add_auth_test(mock_config)
        # Cycle 1: the parent plus two replies.
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [
                    {"ts": "111.222", "text": "the request"},
                    {"ts": "111.333", "text": "first"},
                    {"ts": "111.444", "text": "second"},
                ],
            },
            status=200,
        )
        # Cycle 2: "first" was deleted, "third" arrived.
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={
                "ok": True,
                "messages": [
                    {"ts": "111.222", "text": "the request"},
                    {"ts": "111.444", "text": "second"},
                    {"ts": "111.555", "text": "third"},
                ],
            },
            status=200,
        )
        # Cycle 1: no reactions; cycle 2: an ack ends the watch.
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={"ok": True, "message": {"reactions": []}},
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "reactions.get"),
            json={
                "ok": True,
                "message": {"reactions": [{"name": "+1", "users": ["U1"]}]},
            },
            status=200,
        )
        responses.add(
            responses.GET,
            _api(mock_config, "users.info"),
            json={"ok": True, "user": {"profile": {"display_name": "Reviewer R"}}},
            status=200,
        )

        replies: list[str] = []
        outcome = client.wait_for_ack(
            "C123",
            "111.222",
            on_reply=replies.append,
            should_stop=lambda: False,
            interval=0.01,
            timeout=5,
        )

        assert outcome.acked is True
        # "second" is not re-forwarded and "third" is not swallowed.
        assert replies == ["first", "second", "third"]

    @responses.activate
    def test_rate_limit_wait_is_interrupted_by_cancel(self, client, mock_config):
        """A cancel during a 429 Retry-After wait ends the watch after ~1 slice.

        The patched sleep sets the cancel event on its first ~1s slice; the
        next slice's cancel check must abort the wait instead of sleeping out
        the server's 30s, and the watch then ends as timed out.
        """
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"error": "ratelimited"},
            status=429,
            headers={"Retry-After": "30"},
        )

        cancel = threading.Event()
        with patch(
            "mtui.data_sources.slack.time.sleep",
            side_effect=lambda _s: cancel.set(),
        ) as sleep:
            outcome = client.wait_for_ack(
                "C123",
                "111.222",
                on_reply=lambda _t: None,
                should_stop=lambda: False,
                interval=0.01,
                timeout=60,
                cancel_event=cancel,
            )

        assert outcome.acked is False
        assert outcome.timed_out is True
        assert outcome.unreachable is False
        assert sleep.call_count == 1  # one slice, then the cancel is honoured

    @responses.activate
    def test_rate_limit_wait_respects_deadline(self, client, mock_config):
        """A 429 wait past the watch deadline aborts without sleeping at all."""
        responses.add(
            responses.GET,
            _api(mock_config, "conversations.replies"),
            json={"error": "ratelimited"},
            status=429,
            headers={"Retry-After": "30"},
        )

        with patch("mtui.data_sources.slack.time.sleep") as sleep:
            outcome = client.wait_for_ack(
                "C123",
                "111.222",
                on_reply=lambda _t: None,
                should_stop=lambda: False,
                interval=0.01,
                timeout=0,  # the deadline has already passed
            )

        assert outcome.acked is False
        assert outcome.timed_out is True
        sleep.assert_not_called()
