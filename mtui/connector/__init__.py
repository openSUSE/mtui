"""Transitional shim: connector/ is being moved to data_sources/.

Only ``qem_dashboard`` and ``oqa_search`` remain here while their
splits land in subsequent commits. ``Gitea`` and ``OSC`` have moved
to :mod:`mtui.data_sources`.
"""

from .qem_dashboard import DashboardAutoOpenQA, QEMIncident

__all__ = ["DashboardAutoOpenQA", "QEMIncident"]
