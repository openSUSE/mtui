
from .sle11 import normalize_sle11
from .sle12 import normalize_sle12
from .sle15 import normalize_sle15

from .misc import normalize_rt, normalize_ses, normalize_caasp, normalize_manager, normalize_cloud


def normalize(x):
    # SLERT must be before version based comparsion
    if x[0][0] == 'SLE-RT':
        return normalize_rt(x)
    # SLE 11 products
    if x[0][1].startswith('11'):
        return normalize_sle11(x)
    # SLE 12 Products
    if x[0][1].startswith('12'):
        return normalize_sle12(x)

    if x[0][1].startswith('15'):
        return normalize_sle15(x)

    if x[0][0] == 'SUSE-CAASP':
        return normalize_caasp(x)
    if x[0][0] == 'Storage':
        return normalize_ses(x)
    if 'OpenStack-Cloud' in x[0][0]:
        return normalize_cloud(x)
    if 'SUSE-Manager' in x[0][0] or 'SLE-Manager-Tools' in x[0][0]:
        return normalize_manager(x)
    if 'SLE-STUDIOONSITE' in x[0][0]:
        x[0][0] = x[0][0].lower()
    if 'SLE-WEBYAST' in x[0][0]:
        x[0][0] = 'sle-11-WebYaST'
    # Cornercases ..
    return x
