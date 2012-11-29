#!/usr/bin/env python

from distutils.core import setup
import distutils.command.build
import distutils.command.install_data
import os.path
import sys

data_files = []

setup(name='mtui',
      version = '1.0',
      description = 'Maintenance Test Update Installer',
      long_description = 'Command-line client for remote QAM test update installation and tracking.',
      author = 'Christian Kornacker',
      author_email = 'ckornacker@suse.de',
      license = 'GPL',
      platforms = ['Linux', 'Mac OSX'],
      keywords = ['SUSE', 'Maintenance', 'update', 'testing'],
      url = 'http://www.suse.com',
      download_url = 'http://qam.suse.de/infrastructure/mtui/',
      packages = ['mtui'],
      scripts = ['mtui.py'],
      data_files = data_files
      )

