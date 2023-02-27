def normalize_sle11(x):
    """Normalize SLE 11 Products"""
    if x[0][0] == "SLE-SDK":
        x[0][0] = "sle-sdk"
        return x
    if x[0][0] == "SLE-SAP-AIO":
        x[0][0] = "SUSE_SLES_SAP"
        return x
    if x[0][0] == "SLE-SERVER" and (
        x[0][1].split("-")[-1] not in ("TERADATA", "SECURITY", "PUBCLOUD", "CORE")
    ):
        x[0][0] = "SUSE_SLES"
        x[0][1] = x[0][1].replace("-LTSS", "")
        x[0][1] = x[0][1].replace("-CLIENT-TOOLS", "")
        return x
    if x[0][1].endswith("CORE"):
        x[0][0] = "SUSE_SLES_LTSS-EXTREME-CORE"
        x[0][1] = x[0][1].replace("-LTSS-EXTREME-CORE", "")
    if x[0][1].endswith("TERADATA"):
        x[0][0] = "teradata"
        x[0][1] = x[0][1].replace("-TERADATA", "")
        return x
    if x[0][1].endswith("SECURITY"):
        x[0][0] = "security"
        x[0][1] = "11"
        return x
    if x[0][1].endswith("PUBCLOUD"):
        x[0][0] = "sle-module-pubcloud"
        x[0][1] = "11"
        return x
    if x[0][0] == "SLE-SMT":
        x[0][0] = "sle-smt"
        return x
    if x[0][0] == "SLE-HAE":
        x[0][0] = "sle-hae"
        return x
    return x
