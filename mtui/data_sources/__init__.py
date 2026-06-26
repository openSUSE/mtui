"""External data-source clients (Gitea, OSC, openQA, QEM Dashboard, TeReGen)."""

from .gitea import Gitea
from .oscqam import OSC
from .teregen import TeReGen

__all__ = ["OSC", "Gitea", "TeReGen"]
