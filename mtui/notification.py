#
# mtui notifications, currently supporting python-notify only
#
from logging import getLogger

logger = getLogger("mtui.notifications")

__impl = None


def display(
    summary: str | None = None, text: str | None = None, icon: str = "stock_dialog-info"
) -> None:
    global __impl
    if __impl is None:
        try:
            import pynotify as __impl  # type: ignore
        except ImportError:
            __impl = False
            logger.debug("pynotify not installed. notification disabled.")
        else:
            if not __impl.init("mtui"):  # type: ignore
                __impl = False
                logger.debug("failed to initialize pynotify")

    if not __impl:
        return

    logger.debug('displaying notify message "%s"', text)
    try:
        __impl.Notification(summary, text, icon).show()  # type: ignore
    except Exception:
        logger.debug("failed to display notification")
