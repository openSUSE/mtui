"""Module entrypoint enabling ``python -m mtui``.

Mirrors the ``mtui`` console-script registered in ``pyproject.toml``.
"""

from .main import main

if __name__ == "__main__":
    raise SystemExit(main())
