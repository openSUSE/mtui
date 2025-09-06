import re
from dataclasses import dataclass
from datetime import datetime
from functools import total_ordering
from logging import getLogger
from typing import Any, final

import requests
import urllib3
from urllib3.exceptions import InsecureRequestWarning

# The 'Config' object has dynamically generated attributes, so we must
# ignore type checker warnings when accessing them.
from ..config import Config
from ..types import assignment, method
from ..exceptions import (
    FailedGiteaCall,
    GiteaAssignInvalid,
    GiteaNoReview,
    MissingGiteaToken,
)

logger = getLogger("mtui.connector.gitea")

# Suppress insecure request warnings for unverified HTTPS requests.
urllib3.disable_warnings(category=InsecureRequestWarning)


@dataclass
@total_ordering
class Comment:
    """Represents a Gitea comment, sortable by date."""

    serial: int
    body: str
    date: datetime

    def __eq__(self, other: object, /) -> bool:
        if not isinstance(other, Comment):
            return NotImplemented
        return self.date == other.date

    def __gt__(self, other: object, /) -> bool:
        if not isinstance(other, Comment):
            return NotImplemented
        return self.date > other.date

    def __repr__(self) -> str:
        return f"<Comment: {self.serial}>"


@final
class Gitea:
    """
    A client for managing a Gitea Pull Request via a comment-based workflow.

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
        """
        Initializes the Gitea API client.

        Args:
            config: The application configuration object.
            giteaprapi: The full Gitea API URL for the specific pull request.
            group: The review group this instance is operating on behalf of (e.g., 'qam-sle').

        Raises:
            MissingGiteaToken: If the Gitea API token is not found in the config.
        """
        if not config.gitea_token:  # type: ignore
            raise MissingGiteaToken("Gitea API token is empty, can't access API")

        self.user: str = config.session_user  # type: ignore
        self.headers = {"Authorization": f"token {config.gitea_token}"}
        self.group = group

        # Construct the necessary API endpoints from the base PR URL.
        self.pr = giteaprapi
        self.issues = "/".join(giteaprapi.split("/")[:-2]) + "/issues/comments"
        self.prissues = giteaprapi.replace("pulls", "issues") + "/comments"

    def __request(
        self,
        method: method,
        url: str,
        params: dict | None = None,
        data: dict | None = None,
        json: dict | None = None,
        verify: bool = False,
    ) -> Any:
        """
        A private wrapper for making requests to the Gitea API.

        Args:
            method: The HTTP method to use (GET, POST, etc.).
            url: The API endpoint URL.
            params: URL parameters for the request.
            data: Form data for the request body.
            json: JSON data for the request body.
            verify: If True (default), verifies SSL certificates. Set to False only
                    for development environments with self-signed certs.

        Returns:
            The JSON response from the API.

        Raises:
            FailedGiteaCall: If the API call fails due to a network error or an
                             unsuccessful status code.
        """
        try:
            logger.debug("Requesting %s on %s", method, url)
            rsp = requests.request(
                method,
                url,
                headers=self.headers,
                params=params,
                data=data,
                json=json,
                verify=verify,
            )
        except requests.exceptions.RequestException as e:
            logger.error("API call to Gitea failed: %s", e, exc_info=True)
            raise FailedGiteaCall(f"{method} - {url}")

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
            raise FailedGiteaCall(f"{method} - {url} returned status {rsp.status_code}")

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

    def __check_assign(self, check_user: str | None = None) -> assignment:
        """
        Determines the current assignment state by parsing PR comments.

        This method reads all comments, sorts them chronologically, and simulates
        a state machine. The last valid assignment or unassignment comment for
        the configured group determines the final state.

        Args:
            check_user: The user to check the assignment status for.

        Returns:
            The assignment state (UNASSIGNED, ASSIGNED_USER, or ASSIGNED_OTHER).
        """
        # Always reload comments to get the most up-to-date state.
        comments = sorted(self.__get_all_comments())
        assignee: str | None = None

        # Iterate through comments chronologically to find the last state update.
        for c in comments:
            if match := self.ASSIGN_RE.match(c.body):
                user, group = match.groups()
                if group == self.group:
                    assignee = user  # This user is now the assignee.
            elif match := self.UNASSIGN_RE.match(c.body):
                user, group = match.groups()
                if group == self.group:
                    assignee = None  # The PR is now unassigned for this group.

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

    def approve(self, other: str | None = None) -> None:
        """
        Approves the PR by posting a comment, if currently assigned to the user.

        Args:
            other: Username to act on behalf of. Defaults to the session user.

        Raises:
            GiteaAssignInvalid: If the PR is not assigned to the specified user.
        """
        a_user = other if other else self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalid(assign_state, a_user)

        logger.info("Approving PR as %s for group %s", a_user, self.group)
        msg = f"@{self.group}-review: approve\n\nLGTM"
        self.__request(method.POST, self.prissues, json={"body": msg})

    def reject(
        self, reason: str = "", other: str | None = None, message: str = ""
    ) -> None:
        """
        Rejects the PR by posting a comment, if currently assigned to the user.

        Args:
            reason: An optional reason for the rejection.
            other: Username to act on behalf of. Defaults to the session user.
            message: Message from user

        Raises:
            GiteaAssignInvalid: If the PR is not assigned to the specified user.
        """
        a_user = other if other else self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalid(assign_state, a_user)

        logger.info("Rejecting PR as %s for group %s", a_user, self.group)
        msg = f"@{self.group}-review: decline"
        if reason:
            msg += f"\nReason: {reason}"
        if message:
            msg += f"\n{message}"
        self.__request(method.POST, self.prissues, json={"body": msg})

    def assign(self, other: str | None = None, force: bool = False) -> None:
        """
        Assigns the PR to a user by posting an assignment comment.

        Args:
            other: Username to assign. Defaults to the session user.
            force: If True, bypasses the check for an existing review request.

        Raises:
            GiteaNoReview: If a review has already been requested and `force` is False.
            GiteaAssignInvalid: If the PR is not in an unassigned state.
        """
        a_user = other if other else self.user
        if self.__has_review() and not force:
            raise GiteaNoReview(f"There is already a PR for {self.group}")

        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.UNASSIGNED:
            raise GiteaAssignInvalid(assign_state, a_user)

        logger.info("Assigning PR to %s for group %s", a_user, self.group)
        msg = self.ASSIGN_TEMPLATE % (a_user, self.group)
        self.__request(method.POST, self.prissues, json={"body": msg})

    def unassign(self, other: str | None = None) -> None:
        """
        Unassigns a user from the PR by posting an unassignment comment.

        Args:
            other: The username to unassign. Defaults to the session user.

        Raises:
            GiteaAssignInvalid: If the PR is not assigned to the specified user.
        """
        a_user = other if other else self.user
        assign_state = self.__check_assign(a_user)
        if assign_state != assignment.ASSIGNED_USER:
            raise GiteaAssignInvalid(assign_state, a_user)

        logger.info("Unassigning user %s for group %s", a_user, self.group)
        msg = self.UNASSIGN_TEMPLATE % (a_user, self.group)
        self.__request(method.POST, self.prissues, json={"body": msg})

    def comment(self, body: str) -> None:
        """
        Posts a generic comment to the pull request.

        Args:
            body: The text content of the comment.
        """
        logger.info("Posting a comment to Gitea PR")
        self.__request(method.POST, self.prissues, json={"body": body})

    def __repr__(self) -> str:
        return f"<GiteaAPI: {self.pr}>"
