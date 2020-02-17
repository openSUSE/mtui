# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2
#
# mtui notifications, currently supporting python-notify only
#
from logging import getLogger

logger = getLogger('mtui.notifications')

__impl = None


def display(summary=None, text=None, icon='stock_dialog-info'):
    global __impl
    if __impl is None:
        try:
            import pynotify as __impl  # type: ignore
        except ImportError:
            __impl = False
            logger.debug('pynotify not installed. notification disabled.')
        else:
            if not __impl.init('mtui'):
                __impl = False
                logger.debug('failed to initialize pynotify')

    if not __impl:
        return

    logger.debug('displaying notify message "{!s}"'.format(text))
    try:
        __impl.Notification(summary, text, icon).show()
    except Exception:
        logger.debug('failed to display notification')
