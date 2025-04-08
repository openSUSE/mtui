from xdg.BaseDirectory import save_cache_path as x_save_cache_path  # type: ignore
from pathlib import Path

app = "mtui"


def save_cache_path(*args: str) -> Path:
    return Path(x_save_cache_path(app)).joinpath(*args)
