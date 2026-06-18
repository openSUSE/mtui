"""A client for managing Gitea pull requests via a comment-based workflow.

This module defines the `Gitea` class, which provides methods to
assign, unassign, approve, and reject a pull request by posting
specially formatted comments.
"""

import re
from dataclasses import dataclass
from datetime import datetime
from functools import total_ordering
from logging import getLogger
from typing import Any, final, override
from urllib.parse import urlparse

import requests

# The 'Config' object has dynamically generated attributes, so we must
# ignore type checker warnings when accessing them.
from ..support.config import Config
from ..support.exceptions import (
    FailedGiteaCallError,
    GiteaAssignInvalidError,
    GiteaNoReviewError,
    MissingGiteaTokenError,
)
from ..support.http import HTTP_TIMEOUT, VerifyPolicy, build_session, resolve_verify
from ..types import assignment, method

logger = getLogger("mtui.connector.gitea")


def pr_api_url(web_url: str) -> str:
    """Convert a Gitea PR *web* URL to its REST *API* URL.

    ``https://<host>/<owner>/<repo>/pulls/<n>`` becomes
    ``https://<host>/api/v1/repos/<owner>/<repo>/pulls/<n>`` — the form the
    :class:`Gitea` constructor expects. The SLFO update feed only carries the
    web form (an update's ``external_url``), so callers that build a client
    straight from the feed need this conversion.

    Raises:
        ValueError: If ``web_url`` is not a recognisable Gitea PR URL.

    """
    parsed = urlparse(web_url)
    parts = parsed.path.strip("/").split("/")
    if len(parts) < 4 or parts[-2] != "pulls":
        raise ValueError(f"not a Gitea PR URL: {web_url}")
    owner, repo, _, number = parts[-4:]
    return (
        f"{parsed.scheme}://{parsed.netloc}/api/v1/repos/{owner}/{repo}/pulls/{number}"
    )


@dataclass
@total_ordering
class Comment:
    """Represents a Gitea comment, sortable by date."""

    serial: int
    body: str
    date: datetime

    @override
    def __eq__(self, other: object, /) -> bool:
        """Checks if two comments are equal."""
        if not isinstance(other, Comment):
            return NotImplemented
        return self.date == other.date

    @override
    def __hash__(self) -> int:
        """Hashes the comment by serial."""
        return hash(self.serial)

    def __gt__(self, other: object, /) -> bool:
        """Checks if this comment is greater than another."""
        if not isinstance(other, Comment):
            return NotImplemented
        return self.date > other.date

    @override
    def __repr__(self) -> str:
        """Returns a string representation of the comment."""
        return f"<Comment: {self.serial}>"

    @override
    def __str__(self) -> str:
        return self.body


@final
class Gitea:
    """A client for managing a Gitea Pull Request via a comment-based workflow.

    This class provides methods to assign, unassign, approve, and reject a PR
    by posting specially formatted comments. It determines the current state
    by parsing the entire comment history for the PR.
    """

    # Template for a comment indicating a user has been assigned to the PR for a group.
    ASSIGN_TEMPLATE = "<MTUI: PR - UV assigned to user: %s - group: %s >"
    # Template for a comment indicating a user has been unassigned.
    UNASSIGN_TEMPLATE = "<MTUI: PR - UV unassigned user: %s - group: %s >"

    # Compiled regex to find and parse assignment comments.
    ASSIGN_RE = re.compile(
        r"<MTUI: PR - UV assigned to user: (?P<user>.*) - group: (?P<group>.*) >"
    )
    # Compiled regex to find and parse unassignment comments.
    UNASSIGN_RE = re.compile(
        r"<MTUI: PR - UV unassigned user: (?P<user>.*) - group: (?P<group>.*) >"
    )

    def __init__(self, config: Config, giteaprapi: str, group: str = "qam-sle") -> None:
        """Initializes the Gitea API client.

        Args:
            config: The application configuration object.
            giteaprapi: The full Gitea API URL for the specific pull request.
            group: The review group this instance is operating on behalf of.

        Raises:
            MissingGiteaTokenError: If the Gitea API token is not found in the config.

        """
        if not config.gitea_token:
            raise MissingGiteaTokenError("Gitea API token is empty, can't access API")

        self.user: str = config.session_user
        self.headers = {"Authorization": f"token {config.gitea_token}"}
        self.group = group

        # Resolve the TLS verification policy once: verify by default and
        # let the global ``[mtui] ssl_verify`` policy override (disable
        # verification or point at a CA bundle). The shared session
        # silences the InsecureRequestWarning when verification is off.
        self._verify: VerifyPolicy = resolve_verify(True, config.ssl_verify)
        self._session = build_session(self._verify)

        # Construct the necessary API endpoints from the base PR URL.
        self.pr = giteaprapi
        self.prissues = giteaprapi.replace("pulls", "issues") + "/comments"

    def __request(
        self,
        method: method,
        url: str,
        params: dict[str, Any] | None = None,
        data: dict[str, Any] | None = None,
        json: dict[str, Any] | None = None,
    ) -> Any:
        """A private wrapper for making requests to the Gitea API.

        Args:
            method: The HTTP method to use (GET, POST, etc.).
            url: The API endpoint URL.
            params: URL parameters for the request.
            data: Form data for the request body.
            json: JSON data for the request body.

        Returns:
            The JSON response from the API.

        Raises:
            FailedGiteaCallError: If the API call fails.

        """
        try:
            logger.debug("Requesting %s on %s", method, url)
            rsp = self._session.request(
                method,
                url,
                headers=self.headers,
                params=params,
                data=data,
                json=json,
                timeout=HTTP_TIMEOUT,
            )
        except requests.exceptions.RequestException as e:
            logger.exception("API call to Gitea failed: %s", e)
            raise FailedGiteaCallError(f"{method} - {url}") from e

        if not rsp.ok:
            logger.warning(
                "API call to %s failed with status code: %s", url, rsp.status_code
            )
            # Try to get a more specific error message from the response body.
            try:
                if msg := rsp.json().get("message"):
                    logger.debug("Gitea error message: %s", msg)
            except requests.exceptions.JSONDecodeError:
                logger.debug("No JSON error message in response.")
            raise FailedGiteaCallError(
                f"{method} - {url} returned status {rsp.status_code}"
            )

        # For 204 No Content, there is no body to decode.
        if rsp.status_code == 204:
            return []
        return rsp.json()

    def __get_all_comments(self) -> list[Comment]:
        """Fetches and deserializes all comments on the pull request."""
        cmts = self.__request(method.GET, self.prissues)
        return [
            Comment(c["id"], c["body"], datetime.fromisoformat(c["updated_at"]))
            for c in cmts
        ]

    @classmethod
    def _assignee_from_comments(cls, comments: list[Comment], group: str) -> str | None:
        """Replay assign/unassign markers and return the current assignee.

        Simulates a state machine over the comments for ``group``: the last
        valid assignment or unassignment marker wins. Returns the assignee's
        username, or ``None`` when the group is currently unassigned.

        Args:
            comments: PR comments sorted chronologically (oldest first).
            group: The review group whose markers to honour.

        """
        assignee: str | None = None
        for c in comments:
            if (match := cls.ASSIGN_RE.match(c.body)) and match.group("group") == group:
                assignee = match.group("user")  # This user is now the assignee.
            elif (match := cls.UNASSIGN_RE.match(c.body)) and match.group(
                "group"
            ) == group:
                assignee = None  # The PR is now unassigned for this group.
        return assignee

    def assignee(self) -> str | None:
        """Return the current assignee for this PR's group, or ``None``.

        Reloads the comments and replays the assign/unassign markers (see
        :meth:`_assignee_from_comments`). ``None`` means the group is
        unassigned — no marker, or the last marker is an unassignment.
        """
        return self._assignee_from_comments(
            sorted(self.__get_all_comments()), self.group
        )

    def __check_assign(self, check_user: str | None = None) -> assignment:
        """Determines the current assignment state by parsing PR comments.

        Args:
            check_user: The user to check the assignment status for.

        Returns:
            The assignment state.

        """
        assignee = self.assignee()
        if assignee is None:
            return assignment.UNASSIGNED
        if assignee == check_user:
            return assignment.ASSIGNED_USER
        return assignment.ASSIGNED_OTHER

    def __has_review(self) -> bool:
        """Checks if a review has been requested for the configured group."""
        pr_data = self.__request(method.GET, self.pr)
        if reviewers := pr_data.get("requested_reviewers"):
            return any(d.get("login") == f"{self.group}-review" for d in reviewers)
        return False

    def __is_done(self) -> bool:
        """Checks if the pull request has been approved or rejected."""
        comments = sorted(self.__get_all_comments())

        done = re.compile(f"@{self.group}-review: (LGTM|approved?|declined?)")
        return any(done.match(c.body) for c in comments)

    def approve(self, other: str | None = None) -> None:
        """Approves the PR by posting a comment.

        Args:
            other: Username to act on behalf of. Defaults to the session user.

        Raises:
            GiteaAssignInvalidError: If the PR is not assigned to the specified user.
            GiteaNoReviewError: If the PR was already approved/rejected.

        """
        a_user = other or self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalidError(assign_state, a_user)

        if self.__is_done():
            raise GiteaNoReviewError("PR was already approved/rejected")

        logger.info("Approving PR as %s for group %s", a_user, self.group)
        msg = f"@{self.group}-review: LGTM"
        self.__request(method.POST, self.prissues, json={"body": msg})

    def reject(
        self, reason: str = "", other: str | None = None, message: str = ""
    ) -> None:
        """Rejects the PR by posting a comment.

        Args:
            reason: An optional reason for the rejection.
            other: Username to act on behalf of. Defaults to the session user.
            message: Message from user.

        Raises:
            GiteaAssignInvalidError: If the PR is not assigned to the specified user.
            GiteaNoReviewError: If the PR was already approved/rejected.

        """
        a_user = other or self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalidError(assign_state, a_user)

        if self.__is_done():
            raise GiteaNoReviewError("PR was already approved/rejected")

        logger.info("Rejecting PR as %s for group %s", a_user, self.group)
        msg = f"@{self.group}-review: decline"
        if reason:
            msg += f"\nReason: {reason}"
        if message:
            msg += f"\n{message}"
        self.__request(method.POST, self.prissues, json={"body": msg})

    def assign(self, other: str | None = None, force: bool = False) -> None:
        """Assigns the PR to a user by posting an assignment comment.

        Args:
            other: Username to assign. Defaults to the session user.
            force: If True, bypasses the check for an existing review request.

        Raises:
            GiteaNoReviewError: If a review has not been requested and `force`
                is False, or if the PR was already approved/rejected.
            GiteaAssignInvalidError: If the PR is not in an unassigned state
                and `force` is False.

        """
        a_user = other or self.user
        # `force` skips the "review requested" and "already assigned" guards
        # and simply posts the assignment comment -- e.g. to (re)assign a PR
        # that is currently assigned to a different user. An approved/rejected
        # PR is still refused.
        if not force and not self.__has_review():
            raise GiteaNoReviewError(f"There is no review for {self.group}-review")

        if self.__is_done():
            raise GiteaNoReviewError("PR was already approved/rejected")

        if not force:
            assign_state = self.__check_assign(a_user)
            if assign_state != assignment.UNASSIGNED:
                raise GiteaAssignInvalidError(assign_state, a_user)

        logger.info("Assigning PR to %s for group %s", a_user, self.group)
        msg = self.ASSIGN_TEMPLATE % (a_user, self.group)
        self.__request(method.POST, self.prissues, json={"body": msg})

    def unassign(self, other: str | None = None) -> None:
        """Unassigns a user from the PR by posting an unassignment comment.

        Args:
            other: The username to unassign. Defaults to the session user.

        Raises:
            GiteaAssignInvalidError: If the PR is not assigned to the specified user.

        """
        a_user = other or self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalidError(assign_state, a_user)

        logger.info("Unassigning user %s for group %s", a_user, self.group)
        msg = self.UNASSIGN_TEMPLATE % (a_user, self.group)
        self.__request(method.POST, self.prissues, json={"body": msg})

    def comment(self, body: str) -> None:
        """Posts a generic comment to the pull request.

        Args:
            body: The text content of the comment.

        """
        logger.info("Posting a comment to Gitea PR")
        self.__request(method.POST, self.prissues, json={"body": body})

    def get_hash(self) -> str:
        data = self.__request(method.GET, self.pr)
        return data["head"]["sha"]

    @override
    def __repr__(self) -> str:
        """Returns a string representation of the Gitea object."""
        return f"<GiteaAPI: {self.pr}>"
