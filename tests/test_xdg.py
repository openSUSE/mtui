import pytest
from mtui import xdg

from unittest.mock import patch
from pathlib import Path

@patch('mtui.xdg.x_save_cache_path', return_value='/tmp/cache')
def test_save_cache_path(mock_save_cache_path):
    """
    Test save_cache_path
    """
    path = xdg.save_cache_path("test", "file")
    assert path == Path("/tmp/cache/test/file")
