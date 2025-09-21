"""A function for normalizing SLE 12 product information."""


def normalize_sle12(x):
    """Normalizes SLE 12 product information.

    Args:
        x: A tuple containing the product information.

    Returns:
        The normalized product information.
    """
    if x[0][0] == "SLE-SERVER" and "LTSS-Extended-Security" in x[0][1]:
        x[0][0] = "SLES-LTSS-Extended-Security"
        x[0][1] = x[0][1].replace("-LTSS-Extended-Security", "")
        return x
    if x[0][0] == "SLE-SERVER" and "LTSS-ERICSSON" in x[0][1]:
        x[0][0] = "SLES-LTSS-ERICSSON"
        x[0][1] = x[0][1].replace("-LTSS-ERICSSON", "")
        return x
    if x[0][0] == "SLE-SERVER" and "LTSS-SAP" in x[0][1]:
        x[0][0] = "SLES-LTSS-SAP"
        x[0][1] = x[0][1].replace("-LTSS-SAP", "")
        return x
    if x[0][0] == "SLE-SERVER" and "LTSS-TERADATA" in x[0][1]:
        x[0][0] = "SLES_LTSS_TERADATA"
        x[0][1] = x[0][1].replace("-LTSS-TERADATA", "")
        return x
    if x[0][0] == "SLE-SERVER" and "LTSS" in x[0][1]:
        x[0][0] = "SLES-LTSS"
        x[0][1] = x[0][1].replace("-LTSS", "")
        return x
    if x[0][0] == "SLE-SERVER" and "TERADATA" in x[0][1]:
        x[0][0] = "SLES_TERADATA"
        x[0][1] = x[0][1].replace("-TERADATA", "")
        return x
    if x[0][0] == "SLE-SERVER":
        x[0][0] = "SLES"
        return x
    if x[0][0] == "SLE-DESKTOP":
        x[0][0] = "SLED"
        return x
    if x[0][0] == "SLE-RPI":
        x[0][0] = "SLES_RPI"
        return x
    if x[0][0] == "SLE-SAP":
        x[0][0] = "SLES_SAP"
        return x
    # All other SLE12 modules/extensions in lowercase
    x[0][0] = x[0][0].lower()
    return x
