"""A collection of functions for normalizing miscellaneous product information."""


def normalize_ses(x):
    """Normalizes SES product information.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    x[0][0] = "ses"
    return x


def normalize_rt(x):
    """Normalizes SLES-RT product information.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    x[0][0] = "SUSE-Linux-Enterprise-RT"
    return x


def normalize_manager(x):
    """Normalizes SUSE Manager product information.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    if x[0][0] == "SLE-Manager-Tools":
        x[0][0] = "sle-manager-tools"
        return x
    return x


def normalize_osle(x):
    """Normalizes openSUSE Leap product information.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    x[0][0] = "leap"
    x[0][1] = x[0][2]
    x[0][2] = "x86_64"
    return x
