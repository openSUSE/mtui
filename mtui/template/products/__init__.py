"""A collection of functions for normalizing product information."""

from .misc import (
    normalize_manager,
    normalize_osle,
    normalize_rt,
    normalize_ses,
)
from .sle11 import normalize_sle11
from .sle12 import normalize_sle12
from .sle15 import normalize_sle15


def normalize(x):
    """Normalizes product information based on the product name and version.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    # SLERT must be before version based comparsion
    if x[0][0] == "SLE-RT":
        return normalize_rt(x)
    # SLE 11 products
    if x[0][1].startswith("11"):
        return normalize_sle11(x)
    # SLE 12 Products
    if x[0][1].startswith("12"):
        return normalize_sle12(x)

    if x[0][1].startswith("15"):
        return normalize_sle15(x)

    if x[0][0] == "Storage":
        return normalize_ses(x)
    if "SUSE-Manager" in x[0][0] or "SLE-Manager-Tools" in x[0][0]:
        return normalize_manager(x)
    if "openSUSE-SLE" in x[0][1]:
        return normalize_osle(x)
    # Cornercases ..
    return x
