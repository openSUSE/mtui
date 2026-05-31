"""This package contains the openQA connector classes for mtui.

Each module in this package defines a connector to a specific
openQA workflow, such as the auto or kernel workflow.
"""

from .kernel import KernelOpenQA
from .standard import AutoOpenQA

__all__ = ["AutoOpenQA", "KernelOpenQA"]
