from pathlib import Path

from mtui import xdg


def test_save_cache_path(monkeypatch):
    """Test save_cache_path."""
    monkeypatch.setattr("mtui.xdg.x_save_cache_path", lambda _: "/tmp/cache")
    path = xdg.save_cache_path("test", "file")
    assert path == Path("/tmp/cache/test/file")
