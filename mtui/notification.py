# -*- coding: utf-8 -*-
#
# mtui notifications, currently supporting python-notify only
#

import logging
import warnings

try:
    with warnings.catch_warnings():
        warnings.filterwarnings('ignore')
        import pynotify

    if not pynotify.init('mtui'):
        logging.getLogger('mtui').debug('failed to initialize pynotify')
        del pynotify

except ImportError:
    pass


class Notification(object):
    def __init__(self, logger, summary=None, text=None, icon='stock_dialog-info'):
        self.log = logger
        self.summary = summary
        self.text = text
        self.icon = icon
        try:
            self.notify = pynotify.Notification(self.summary, self.text, self.icon)
        except NameError:
            pass

    def show(self):
        self.log.debug('displaying notify message "%s"' % self.text)
        try:
            self.notify.show()
        except AttributeError:
            self.log.debug('pynotify not installed. notification disabled.')
        except Exception:
            self.log.debug('failed to display notification')
