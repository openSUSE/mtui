# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2
#
# mtui notifications, currently supporting python-notify only
#


__impl = None


def display(log, summary=None, text=None, icon='stock_dialog-info'):
    global __impl
    if __impl is None:
        try:
            import pynotify as __impl
        except ImportError:
            __impl = False
            log.debug('pynotify not installed. notification disabled.')
        else:
            if not __impl.init('mtui'):
                __impl = False
                log.debug('failed to initialize pynotify')

    if not __impl:
        return

    log.debug('displaying notify message "%s"' % text)
    try:
        __impl.Notification(summary, text, icon).show()
    except Exception as e:
        log.debug('failed to display notification')
