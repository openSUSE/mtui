"""A helper function for saving files in the user's cache directory."""

from xdg.BaseDirectory import save_cache_path as x_save_cache_path  # type: ignore
from pathlib import Path

app = "mtui"


def save_cache_path(*args: str) -> Path:
    """Returns a path to a file in the user's cache directory.

    Args:
        *args: The path components to join to the cache directory.

    Returns:
        A `Path` object representing the full path to the file.
    """
    return Path(x_save_cache_path(app)).joinpath(*args)
