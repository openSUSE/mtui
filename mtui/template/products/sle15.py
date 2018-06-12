
def normalize_sle15(x):
    """ Normalize SLES/D 15SPx products """

    if x[0][0] == "SLE-Product-SLES":
        x[0][0] = "SLES"
        return x
    if x[0][0] == "SLE-Product-SLED":
        x[0][0] = "SLED"
        return x
    if x[0][0] == 'SLE-Product-WE':
        x[0][0] = 'sle-we'
        return x
    if x[0][0] == 'SLE-Product-HA':
        x[0][0] = 'sle-ha'
        return x
    if x[0][0] == 'SLE-Product-HPC':
        x[0][0] = 'sle-hpc'
        return x
    if x[0][0] == 'SLE-Product-SLES_SAP':
        x[0][0] = 'SLES_SAP'
        return x
    # All other SLE12 modules/extensions in lowercase
    x[0][0] = x[0][0].lower()
    return x
