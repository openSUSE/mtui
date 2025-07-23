def normalize_ses(x):
    """Normalize SES"""
    x[0][0] = "ses"
    return x


def normalize_rt(x):
    """Normalize SLES-RT"""
    x[0][0] = "SUSE-Linux-Enterprise-RT"
    return x


def normalize_cloud(x):
    if x[0][0] == "OpenStack-Cloud":
        x[0][0] = "suse-openstack-cloud"
        return x
    if x[0][0] == "OpenStack-Cloud-Magnum-Orchestration":
        x[0][0] = "openstack-cloud-magnum-orchestration"
        return x
    return x


def normalize_manager(x):
    if x[0][0] == "SLE-Manager-Tools":
        x[0][0] = "sle-manager-tools"
        return x
    return x


def normalize_osle(x):
    x[0][0] = "leap"
    x[0][1] = x[0][2]
    x[0][2] = "x86_64"
    return x
