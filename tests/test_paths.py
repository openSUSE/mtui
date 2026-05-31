from pathlib import Path

from mtui.support import paths


def test_save_cache_path(monkeypatch):
    """Test save_cache_path."""
    monkeypatch.setattr("mtui.support.paths.x_save_cache_path", lambda _: "/tmp/cache")
    path = paths.save_cache_path("test", "file")
    assert path == Path("/tmp/cache/test/file")
