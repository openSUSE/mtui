import pytest
from mtui import parsemetajson

from unittest.mock import MagicMock

def test_json_parser_parse():
    """
    Test JSONParser.parse
    """
    results = MagicMock()
    results.jira = {}
    results.bugs = {}

    data = {
        "jira": ["ABC-123"],
        "bugs": ["123"],
        "rrid": "SUSE:Maintenance:1:1",
        "packager": "test_packager",
        "rating": "test_rating",
        "repository": "test_repository",
        "category": "test_category",
        "testplatform": ["test_platform"],
        "products": ["test_product"],
        "id": "test_id",
        "gitea_pr": "test_gitea_pr",
        "gitea_pr_api": "test_gitea_pr_api",
        "packages": {
            "test_prod": ["test_pkg 1.0 1.0"]
        },
        "repositories": ["test_repo"]
    }

    parsemetajson.JSONParser.parse(results, data)

    assert results.jira["ABC-123"] == "Description not available"
    assert results.bugs["123"] == "Description not available"
    assert str(results.rrid) == "SUSE:Maintenance:1:1"
    assert results.packager == "test_packager"
    assert results.rating == "test_rating"
    assert results.repository == "test_repository"
    assert results.category == "test_category"
    assert results.testplatforms == ["test_platform"]
    assert results.products == ["test_product"]
    assert results.realid == "test_id"
    assert results.giteapr == "test_gitea_pr"
    assert results.giteaprapi == "test_gitea_pr_api"
    assert results.packages["test_prod"]["test_pkg"] == "1.0"
    assert results.repositories == frozenset(["test_repo"])
