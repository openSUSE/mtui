#!/usr/bin/python
# -*- coding: utf-8 -*-

import ConfigParser
import os
import getpass

class Config(object):

    """Read the variables from ~/.mtuirc"""

    def __init__(self):
        self.config = ConfigParser.ConfigParser()
        self.config.read(os.path.expanduser('~/.mtuirc'))

    def get_user(self):

        """Return value of the user"""

        try:
            user = self.config.get('mtui', 'user')
        except ConfigParser.NoSectionError:
            user = getpass.getuser()
        return user
