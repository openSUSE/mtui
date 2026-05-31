"""External data-source clients (Gitea, OSC, openQA, QEM Dashboard)."""

from .gitea import Gitea
from .oscqam import OSC

__all__ = ["OSC", "Gitea"]
