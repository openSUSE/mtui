
def normalize_sle15(x):
    """ Normalize SLES/D 15SPx products
    Now probadly same rules as SLE12SPx+-"""
    if x[0][0] == "SLE-SERVER" and "LTSS" in x[0][1]:
        x[0][0] = "SLES-LTSS"
        x[0][1] = x[0][1].replace("-LTSS", "")
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
    if x[0][0] == 'SLE-SAP':
        x[0][0] = 'SLES_SAP'
        return x
    # All other SLE12 modules/extensions in lowercase
    x[0][0] = x[0][0].lower()
    return x
