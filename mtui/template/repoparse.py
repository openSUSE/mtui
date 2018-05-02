
from os.path import join

import xml.etree.ElementTree as ET
from mtui.types import Product


def _read_project(path):
    with open(join(path, 'project.xml'), mode='r') as f:
        return ET.fromstringlist(f)


def _xmlparse(xml):
    return ((x.find('releasetarget').attrib['project'].split(':')[-3:], x.attrib['name'])
            for x in xml.findall("repository/path[@repository='update']/..") if 'DEBUG' not in x.attrib['name'])


def _normalize_sle11(x):
    """ Normalize SLE 11 Products """

    if x[0][0] == 'SLE-SDK':
        x[0][0] = 'sle-sdk'
        return x
    if x[0][0] == 'SLE-SAP-AIO':
        x[0][0] = 'SUSE_SLES_SAP'
        return x
    if x[0][0] == 'SLE-SERVER' and (x[0][1].split('-')[-1] not in ('TERADATA', 'SECURITY', 'PUBCLOUD')):
        x[0][0] = 'SUSE_SLES'
        x[0][1] = x[0][1].replace('-LTSS', '')
        x[0][1] = x[0][1].replace('-CLIENT-TOOLS', '')
        return x
    if x[0][1].endswith('TERADATA'):
        x[0][0] = "teradata"
        x[0][1] = x[0][1].replace('-TERADATA', '')
        return x
    if x[0][1].endswith('SECURITY'):
        x[0][0] = 'security'
        x[0][1] = '11'
        return x
    if x[0][1].endswith('PUBCLOUD'):
        x[0][0] = 'sle-module-pubcloud'
        x[0][1] = '11'
        return x
    if x[0][0] == 'SLE-SMT':
        x[0][0] = 'sle-smt'
        return x
    return x


def _normalize_sle12(x):
    """ Normalize SLES/D 12SPx products"""
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


def _normalize_caasp(x):
    """Normalize CAASP"""
    x[0][0] = 'CAASP'
    x[0][1] = ''
    return x


def _normalize_ses(x):
    """Normalize SES"""
    x[0][0] = 'ses'
    return x


def _normalize_rt(x):
    """Normalize SLES-RT"""
    x[0][0] = 'SUSE-Linux-Enterprise-RT'
    return x


def _normalize_cloud(x):
    if x[0][0] == 'OpenStack-Cloud':
        x[0][0] = 'suse-openstack-cloud'
        return x
    if x[0][0] == 'OpenStack-Cloud-Magnum-Orchestration':
        x[0][0] = 'openstack-cloud-magnum-orchestration'
        return x
    return x


def _normalize_manager(x):
    if x[0][0] == 'SLE-Manager-Tools':
        x[0][0] = 'sle-manager-tools'
        return x
    return x


def _normalize(x):
    # SLERT must be before version based comparsion
    if x[0][0] == 'SLE-RT':
        return _normalize_rt(x)
    if x[0][1].startswith('11'):
        return _normalize_sle11(x)
    if x[0][1].startswith('12'):
        return _normalize_sle12(x)
    if x[0][0] == 'SUSE-CAASP':
        return _normalize_caasp(x)
    if x[0][0] == 'Storage':
        return _normalize_ses(x)
    if 'OpenStack-Cloud' in x[0][0]:
        return _normalize_cloud(x)
    if 'SUSE-Manager' in x[0][0] or 'SLE-Manager-Tools' in x[0][0]:
        return _normalize_manager(x)
    if 'SLE-STUDIOONSITE' in x[0][0]:
        x[0][0] = x[0][0].lower()
    if 'SLE-WEBYAST' in x[0][0]:
        x[0][0] = 'sle-11-WebYaST'
    # Cornercases ..
    return x


def repoparse(path):
    project = _xmlparse(_read_project(path))
    return {Product(x[0], x[1], x[2]): y for x, y in map(_normalize, project)}
