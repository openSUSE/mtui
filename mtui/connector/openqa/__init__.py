"""This package contains the openQA connector classes for mtui.

Each module in this package defines a connector to a specific
openQA workflow, such as the auto or kernel workflow.
"""

from .standard import AutoOpenQA
from .kernel import KernelOpenQA

__all__ = ["AutoOpenQA", "KernelOpenQA"]
