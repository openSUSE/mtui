def normalize_sle15(x):
    """Normalize SLES/D 15SPx products"""
    if x[0][0] == "SLE-Product-SLES" and "LTSS-TERADATA" in x[0][1]:
        x[0][0] = "SLES-LTSS-TERADATA"
        x[0][1] = x[0][1].replace("-LTSS-TERADATA", "")
        return x
    if x[0][0] == "SLE-Product-SLES" and "LTSS" in x[0][1]:
        x[0][0] = "SLES-LTSS"
        x[0][1] = x[0][1].replace("-LTSS", "")
        return x
    if x[0][0] == "SLE-Product-SLES" and "ERICSSON" in x[0][1]:
        x[0][0] = "ERICSSON"
        x[0][1] = x[0][1].replace("-ERICSSON", "")
        return x
    if x[0][0] == "SLE-Product-SLES":
        x[0][0] = "SLES"
        return x
    if x[0][0] == "SLE-Product-SLED":
        x[0][0] = "SLED"
        return x
    if x[0][0] == "SLE-Product-WE":
        x[0][0] = "sle-we"
        return x
    if x[0][0] == "SLE-Product-HA":
        x[0][0] = "sle-ha"
        return x
    if x[0][0] == "SLE-Product-HPC":
        x[0][0] = "SLE_HPC"
        return x
    if x[0][0] == "SLE-Product-SLES_SAP":
        x[0][0] = "SLES_SAP"
        return x
    if x[0][0] == "SLE-Product-RT":
        x[0][0] = "SLE_RT"
        return x
    # All other SLE12 modules/extensions in lowercase
    x[0][0] = x[0][0].lower()
    return x
