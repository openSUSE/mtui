from unittest.mock import MagicMock

from mtui import parsemeta


def test_reduced_metadata_parser_parse():
    """
    Test ReducedMetadataParser.parse
    """
    results = MagicMock()
    results.hostnames = set()
    results.jira = {}
    results.bugs = {}

    # Test hostname parsing
    parsemeta.ReducedMetadataParser.parse(
        results, "some text (reference host: test_host)"
    )
    assert "test_host" in results.hostnames

    # Test Jira issue parsing
    parsemeta.ReducedMetadataParser.parse(results, 'Jira ABC-123 ("Test Jira issue"):')
    assert results.jira["ABC-123"] == "Test Jira issue"

    # Test bug parsing
    parsemeta.ReducedMetadataParser.parse(results, 'Bug 123 ("Test bug"):')
    assert results.bugs["123"] == "Test bug"
