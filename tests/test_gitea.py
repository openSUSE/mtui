"""Tests for the mtui connector gitea module."""

from datetime import datetime
from unittest.mock import MagicMock

import pytest
import responses

from mtui.connector.gitea import Comment, Gitea
from mtui.exceptions import (
    FailedGiteaCallError,
    GiteaAssignInvalidError,
    GiteaNoReviewError,
    MissingGiteaTokenError,
)
from mtui.types import assignment

# --- Comment dataclass ---


class TestComment:
    def test_creation(self):
        """Test Comment dataclass creation."""
        c = Comment(1, "hello", datetime(2024, 1, 1))
        assert c.serial == 1
        assert c.body == "hello"

    def test_equality(self):
        """Test Comments are equal when dates are equal."""
        c1 = Comment(1, "a", datetime(2024, 1, 1))
        c2 = Comment(2, "b", datetime(2024, 1, 1))
        assert c1 == c2

    def test_ordering(self):
        """Test Comments are ordered by date."""
        c1 = Comment(1, "first", datetime(2024, 1, 1))
        c2 = Comment(2, "second", datetime(2024, 1, 2))
        assert c1 < c2
        assert c2 > c1

    def test_sorting(self):
        """Test Comments can be sorted chronologically."""
        c1 = Comment(1, "first", datetime(2024, 1, 3))
        c2 = Comment(2, "second", datetime(2024, 1, 1))
        c3 = Comment(3, "third", datetime(2024, 1, 2))

        result = sorted([c1, c2, c3])
        assert result[0].serial == 2
        assert result[1].serial == 3
        assert result[2].serial == 1

    def test_repr(self):
        """Test Comment repr."""
        c = Comment(42, "body", datetime(2024, 1, 1))
        assert "42" in repr(c)

    def test_str(self):
        """Test Comment str returns body."""
        c = Comment(1, "hello world", datetime(2024, 1, 1))
        assert str(c) == "hello world"

    def test_eq_with_non_comment(self):
        """Test equality with non-Comment returns NotImplemented."""
        c = Comment(1, "body", datetime(2024, 1, 1))
        assert c.__eq__("not a comment") is NotImplemented

    def test_gt_with_non_comment(self):
        """Test gt with non-Comment returns NotImplemented."""
        c = Comment(1, "body", datetime(2024, 1, 1))
        assert c.__gt__("not a comment") is NotImplemented


# --- Gitea initialization ---


class TestGiteaInit:
    def test_missing_token_raises(self):
        """Test init raises MissingGiteaTokenError when token is empty."""
        config = MagicMock()
        config.gitea_token = ""

        with pytest.raises(MissingGiteaTokenError):
            Gitea(config, "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1")

    def test_init_constructs_urls(self, mock_config):
        """Test init constructs API URLs correctly."""
        api_url = "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1"
        gitea = Gitea(mock_config, api_url)  # type: ignore[arg-type]

        assert gitea.pr == api_url
        assert "issues/comments" in gitea.issues
        assert "issues" in gitea.prissues
        assert gitea.user == "testuser"
        assert gitea.group == "qam-sle"

    def test_init_custom_group(self, mock_config):
        """Test init with custom group."""
        api_url = "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1"
        gitea = Gitea(mock_config, api_url, group="qam-kernel")  # type: ignore[arg-type]

        assert gitea.group == "qam-kernel"


# --- Gitea operations (with mocked HTTP) ---


class TestGiteaOperations:
    """Test Gitea operations by mocking the private __request method."""

    @pytest.fixture
    def gitea(self, mock_config):
        api_url = "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1"
        return Gitea(mock_config, api_url)  # type: ignore[arg-type]

    def _make_comment(self, serial, body, date="2024-01-01T00:00:00+00:00"):
        return {"id": serial, "body": body, "updated_at": date}

    @responses.activate
    def test_assign_success(self, gitea):
        """Test successful assignment."""
        # Mock GET comments (check_assign) - no existing comments
        responses.add(
            responses.GET,
            gitea.prissues,
            json=[],
            status=200,
        )
        # Mock GET PR data (has_review)
        responses.add(
            responses.GET,
            gitea.pr,
            json={"requested_reviewers": [{"login": "qam-sle-review"}]},
            status=200,
        )
        # Mock GET comments (is_done)
        responses.add(
            responses.GET,
            gitea.prissues,
            json=[],
            status=200,
        )
        # Mock GET comments (check_assign again for state check)
        responses.add(
            responses.GET,
            gitea.prissues,
            json=[],
            status=200,
        )
        # Mock POST (the assignment comment)
        responses.add(
            responses.POST,
            gitea.prissues,
            json={"id": 1},
            status=201,
        )

        gitea.assign()

    @responses.activate
    def test_assign_no_review_raises(self, gitea):
        """Test assign raises GiteaNoReviewError when no review exists."""
        responses.add(
            responses.GET,
            gitea.pr,
            json={"requested_reviewers": []},
            status=200,
        )

        with pytest.raises(GiteaNoReviewError):
            gitea.assign()

    @responses.activate
    def test_unassign_when_not_assigned_raises(self, gitea):
        """Test unassign raises GiteaAssignInvalidError when not assigned."""
        responses.add(
            responses.GET,
            gitea.prissues,
            json=[],
            status=200,
        )

        with pytest.raises(GiteaAssignInvalidError):
            gitea.unassign()

    @responses.activate
    def test_approve_when_not_assigned_raises(self, gitea):
        """Test approve raises GiteaAssignInvalidError when not assigned."""
        responses.add(
            responses.GET,
            gitea.prissues,
            json=[],
            status=200,
        )

        with pytest.raises(GiteaAssignInvalidError):
            gitea.approve()

    @responses.activate
    def test_comment_posts(self, gitea):
        """Test comment() posts a comment."""
        responses.add(
            responses.POST,
            gitea.prissues,
            json={"id": 1},
            status=201,
        )

        gitea.comment("test comment body")

        assert len(responses.calls) == 1
        body = responses.calls[0].request.body
        if isinstance(body, bytes):
            body = body.decode("utf-8")
        assert body is not None and "test comment body" in body

    @responses.activate
    def test_get_hash(self, gitea):
        """Test get_hash() returns the HEAD SHA."""
        responses.add(
            responses.GET,
            gitea.pr,
            json={"head": {"sha": "abc123def456"}},
            status=200,
        )

        result = gitea.get_hash()
        assert result == "abc123def456"

    @responses.activate
    def test_request_failure_raises_failed_gitea_call(self, gitea):
        """Test API call failures raise FailedGiteaCallError."""
        responses.add(
            responses.GET,
            gitea.pr,
            json={"message": "not found"},
            status=404,
        )

        with pytest.raises(FailedGiteaCallError):
            gitea.get_hash()

    def test_repr(self, gitea):
        """Test Gitea repr."""
        result = repr(gitea)
        assert "GiteaAPI" in result
        assert gitea.pr in result


# --- Assignment state machine ---


class TestAssignmentStateMachine:
    """Test the comment-based state machine for assignment tracking."""

    @pytest.fixture
    def gitea(self, mock_config):
        api_url = "https://gitea.example.com/api/v1/repos/owner/repo/pulls/1"
        return Gitea(mock_config, api_url)  # type: ignore[arg-type]

    @responses.activate
    def test_check_assign_no_comments_returns_unassigned(self, gitea):
        """Test empty comment list means UNASSIGNED."""
        responses.add(responses.GET, gitea.prissues, json=[], status=200)

        # Access private method via name mangling
        result = gitea._Gitea__check_assign("testuser")
        assert result == assignment.UNASSIGNED

    @responses.activate
    def test_check_assign_user_assigned(self, gitea):
        """Test ASSIGNED_USER when user is assigned."""
        comments = [
            {
                "id": 1,
                "body": "<MTUI: PR - UV assigned to user: testuser - group: qam-sle >",
                "updated_at": "2024-01-01T00:00:00+00:00",
            }
        ]
        responses.add(responses.GET, gitea.prissues, json=comments, status=200)

        result = gitea._Gitea__check_assign("testuser")
        assert result == assignment.ASSIGNED_USER

    @responses.activate
    def test_check_assign_other_user_assigned(self, gitea):
        """Test ASSIGNED_OTHER when different user is assigned."""
        comments = [
            {
                "id": 1,
                "body": "<MTUI: PR - UV assigned to user: otheruser - group: qam-sle >",
                "updated_at": "2024-01-01T00:00:00+00:00",
            }
        ]
        responses.add(responses.GET, gitea.prissues, json=comments, status=200)

        result = gitea._Gitea__check_assign("testuser")
        assert result == assignment.ASSIGNED_OTHER

    @responses.activate
    def test_check_assign_unassign_resets_state(self, gitea):
        """Test unassignment comment resets state to UNASSIGNED."""
        comments = [
            {
                "id": 1,
                "body": "<MTUI: PR - UV assigned to user: testuser - group: qam-sle >",
                "updated_at": "2024-01-01T00:00:00+00:00",
            },
            {
                "id": 2,
                "body": "<MTUI: PR - UV unassigned user: testuser - group: qam-sle >",
                "updated_at": "2024-01-02T00:00:00+00:00",
            },
        ]
        responses.add(responses.GET, gitea.prissues, json=comments, status=200)

        result = gitea._Gitea__check_assign("testuser")
        assert result == assignment.UNASSIGNED

    @responses.activate
    def test_check_assign_different_group_ignored(self, gitea):
        """Test assignment comments for different groups are ignored."""
        comments = [
            {
                "id": 1,
                "body": "<MTUI: PR - UV assigned to user: testuser - group: qam-kernel >",
                "updated_at": "2024-01-01T00:00:00+00:00",
            },
        ]
        responses.add(responses.GET, gitea.prissues, json=comments, status=200)

        result = gitea._Gitea__check_assign("testuser")
        assert result == assignment.UNASSIGNED
