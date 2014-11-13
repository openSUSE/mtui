#!/usr/bin/env python

from setuptools import setup
from mtui import __version__

setup(
    name='mtui',
    description = 'Maintenance Test Update Installer',
    long_description = 'Command-line client for remote QAM test '+
        'update installation and tracking.',
    url = 'http://www.suse.com',
    download_url = 'http://qam.suse.de/infrastructure/mtui/',

    version = __version__,

    install_requires = [
        "paramiko",
        'pyxdg',
    ],

    # dependencies not on cheeseshop:
    # rpm (http://www.rpm.org) with python enabled
    # osc (http://en.opensuse.org/openSUSE:OSC)

    extras_require = {
        'keyring': ['keyring'],
    },
    # extra dependencies:
    # notify (http://www.galago-project.org/specs/notification)

    tests_require = [
        'temps',
        'nose',
    ],

    author = 'Christian Kornacker',
    author_email = 'ckornacker@suse.de',

    maintainer = 'SUSE QA Maintenance',
    maintainer_email = 'qa-maintenance@suse.de',

    license = 'License :: Other/Proprietary License',
    platforms = ['Linux', 'Mac OSX'],
    keywords = ['SUSE', 'Maintenance', 'update', 'testing'],

    packages = ['mtui', 'mtui.connector', 'mtui.types'],

    entry_points = {
        'console_scripts': ['mtui = mtui.main:main']},
    scripts = ['refsearch.py']
)
