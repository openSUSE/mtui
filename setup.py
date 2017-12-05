#!/usr/bin/env python3

from setuptools import setup
from setuptools import find_packages
from mtui import loose_version


setup(
    name='mtui',
    description='Maintenance Test Update Installer',
    long_description='Command-line client for remote QAM test ' +
    'update installation and tracking.',
    url='http://www.suse.com',
    download_url='http://qam.suse.de/infrastructure/mtui/',

    version=str(loose_version),

    install_requires=[
        "paramiko",
        'pyxdg',
        'ruamel.yaml'
    ],
    include_package_data=True,

    # dependencies not on cheeseshop:
    # rpm (http://www.rpm.org) with python enabled
    # osc (http://en.opensuse.org/openSUSE:OSC)

    extras_require={
        'keyring': ['keyring'],
    },
    # extra dependencies:
    # notify (http://www.galago-project.org/specs/notification)

    tests_require=[
        'nose'
    ],

    author='Christian Kornacker',
    author_email='ckornacker@suse.de',

    maintainer='SUSE QA Maintenance',
    maintainer_email='qa-maintenance@suse.de',

    license='License :: Other/Proprietary License',
    platforms=['Linux', 'Mac OSX'],
    keywords=['SUSE', 'Maintenance', 'update', 'testing'],
    packages=find_packages(exclude=["*.tests", "*.tests.*", "tests.*", "tests"]),
    entry_points={'console_scripts': ['mtui = mtui.main:main']},

    classifiers=[
        'Programming Language :: Python :: 3', 'Operating System :: POSIX :: Linux', 'Environment :: Console'
    ]
)
