"""External data-source clients (Gitea, OSC, openQA, QEM Dashboard, TeReGen)."""

from .gitea import Gitea
from .oscqam import OSC
from .slack import ReviewOutcome, SlackClient
from .teregen import TeReGen

__all__ = ["OSC", "Gitea", "ReviewOutcome", "SlackClient", "TeReGen"]
