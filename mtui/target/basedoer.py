from abc import ABC, abstractmethod
from logging import getLogger

from ..exceptions import UpdateError
from .actions import ThreadedMethod, queue
from .hostgroup import HostsGroup

logger = getLogger("mtui.targer.baseaction")


class Doer(ABC):
    def __init__(self, targets: HostsGroup, testreport, *args, **kwds) -> None:
        self.targets = targets
        self.testreport = testreport

    def lock_hosts(self) -> None:
        try:
            skipped = False
            for t in self.targets.values():
                if t.is_locked() and not t._lock.is_mine():
                    skipped = True
                    logger.warning(
                        "host %s is locked since %s by %s. skipping.",
                        t.hostname,
                        t._lock.time(),
                        t._lock.locked_by(),
                    )
                    if t._lock.comment():
                        logger.info(
                            "%s's comment: %s", t._lock.locked_by(), t._lock.comment()
                        )
                else:
                    t.lock()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for t in self.targets.values():
                    try:
                        t.unlock()
                    except AssertionError:
                        pass
                raise UpdateError("Hosts locked")
        except BaseException:
            raise

    def unlock_hosts(self) -> None:
        for t in self.targets.values():
            if t.is_locked():
                try:
                    t.unlock()
                except AssertionError:
                    pass

    @abstractmethod
    def run(self, *args, **kwds) -> None: ...
