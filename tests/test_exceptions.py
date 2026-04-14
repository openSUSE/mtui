import pytest

from mtui import exceptions


def test_request_review_id_parse_error():
    """Test RequestReviewIDParseError and its subclasses."""
    with pytest.raises(exceptions.RequestReviewIDParseError, match="test message"):
        raise exceptions.RequestReviewIDParseError("test message")

    with pytest.raises(exceptions.TooManyComponentsError, match="Too many components"):
        exceptions.TooManyComponentsError.raise_if([1, 2, 3], 2)

    with pytest.raises(exceptions.InternalParseError, match="Internal error"):
        raise exceptions.InternalParseError("func", "context")

    with pytest.raises(exceptions.MissingComponentError, match="Missing 1. component"):
        raise exceptions.MissingComponentError(1, "expected")

    with pytest.raises(
        exceptions.ComponentParseError, match="Failed to parse 2. component"
    ):
        raise exceptions.ComponentParseError(2, "expected", "got")


def test_update_error():
    """Test UpdateError."""
    with pytest.raises(exceptions.UpdateError, match="test reason"):
        raise exceptions.UpdateError("test reason")

    with pytest.raises(exceptions.UpdateError, match="test_host: test reason"):
        raise exceptions.UpdateError("test reason", "test_host")


def test_gitea_error():
    """Test GiteaError and its subclasses."""
    from mtui.types import assignment

    with pytest.raises(exceptions.GiteaError):
        raise exceptions.GiteaError()

    with pytest.raises(exceptions.MissingGiteaTokenError):
        raise exceptions.MissingGiteaTokenError()

    with pytest.raises(exceptions.FailedGiteaCallError):
        raise exceptions.FailedGiteaCallError()

    with pytest.raises(exceptions.GiteaNoReviewError):
        raise exceptions.GiteaNoReviewError()

    with pytest.raises(exceptions.GiteaAssignInvalidError, match="different user"):
        raise exceptions.GiteaAssignInvalidError(assignment.ASSIGNED_OTHER, "test_user")
