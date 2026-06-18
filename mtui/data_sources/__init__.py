"""External data-source clients (Gitea, OSC, openQA, QEM Dashboard, SMELT)."""

from .gitea import Gitea
from .oscqam import OSC
from .smelt import Smelt

__all__ = ["OSC", "Gitea", "Smelt"]
