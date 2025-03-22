from abc import ABC, abstractmethod
from collections.abc import Callable
from collections.abc import ValuesView
from pathlib import Path
from queue import Queue
import sys
import threading
from threading import Lock
import time
from typing import Any, Optional

from . import Target
from ..utils import prompt_user

queue: Queue[tuple[Callable[..., None], list[Any]]] = Queue()


class ThreadedMethod(threading.Thread):
    def __init__(self, queue: Queue[tuple[Callable[..., None], list[Any]]]) -> None:
        threading.Thread.__init__(self)
        self.queue = queue

    def run(self) -> None:
        while True:
            try:
                (method, parameter) = self.queue.get(timeout=10)
            except BaseException:
                return

            try:
                method(*parameter)
            except BaseException:
                raise
            finally:
                try:
                    self.queue.task_done()
                except ValueError:
                    pass  # already removed by ctrl+c


class ThreadedTargetGroup(ABC):
    def __init__(self, targets: list[Target] | ValuesView[Target]) -> None:
        self.targets = targets

    def mk_thread(self) -> None:
        thread = ThreadedMethod(queue)
        thread.daemon = True
        thread.start()

    def mk_threads(self) -> None:
        for _ in range(0, len(self.targets)):
            self.mk_thread()

    def run(self) -> None:
        self.mk_threads()
        self.setup_queue()

        while queue.unfinished_tasks:
            spinner()

        queue.join()

    @abstractmethod
    def mk_cmd(self, *args, **kwds) -> tuple[Callable[..., None], list[Any]]:
        pass

    def setup_queue(self) -> None:
        for t in self.targets:
            queue.put(self.mk_cmd(t))


class FileDelete(ThreadedTargetGroup):
    def __init__(self, targets: list[Target] | ValuesView[Target], path: Path) -> None:
        super().__init__(targets)
        self.path = path

    def mk_cmd(self, t: Target):
        return (t.sftp_remove, [self.path])


class FileUpload(ThreadedTargetGroup):
    def __init__(
        self, targets: list[Target] | ValuesView[Target], local: Path, remote: Path
    ) -> None:
        super().__init__(targets)
        self.local = local
        self.remote = remote

    def mk_cmd(self, t: Target):
        return (t.sftp_put, [self.local, self.remote])


class FileDownload(ThreadedTargetGroup):
    def __init__(
        self, targets: list[Target] | ValuesView[Target], remote: Path, local: Path
    ) -> None:
        super().__init__(targets)

        self.remote = remote
        self.local = local

    def mk_cmd(self, t: Target):
        return (t.sftp_get, [self.remote, self.local])


class RunCommand:
    def __init__(
        self, targets: dict[str, Target], command: str | dict[str, Any]
    ) -> None:
        self.targets = targets
        self.command = command

    def run(self) -> None:
        parallel: dict[str, Target] = {}
        serial: dict[str, Target] = {}
        lock = Lock()

        for target in self.targets:
            if self.targets[target].exclusive:
                serial[target] = self.targets[target]
            else:
                parallel[target] = self.targets[target]

        try:
            for target in parallel:
                thread = ThreadedMethod(queue)
                thread.daemon = True
                thread.start()
                if isinstance(self.command, dict):
                    queue.put((parallel[target].run, [self.command[target], lock]))
                elif isinstance(self.command, str):
                    queue.put((parallel[target].run, [self.command, lock]))

            while queue.unfinished_tasks:
                spinner(lock)

            queue.join()

            for target in serial:
                prompt_user(
                    "press Enter key to proceed with {!s}".format(
                        serial[target].hostname
                    ),
                    "",
                )
                thread = ThreadedMethod(queue)
                thread.daemon = True
                thread.start()
                queue.put((serial[target].run, [self.command, lock]))

                while queue.unfinished_tasks:
                    spinner(lock)

                queue.join()

        except KeyboardInterrupt:
            print("stopping command queue, please wait.")
            try:
                while queue.unfinished_tasks:
                    spinner(lock)
            except KeyboardInterrupt:
                for target in self.targets:
                    try:
                        self.targets[target].connection.close_session()
                    except Exception:
                        pass
                try:
                    thread.queue.task_done()  # noqa
                except ValueError:
                    pass

            queue.join()
            print()
            raise


def spinner(lock: Optional[Lock] = None) -> None:
    """simple spinner to show some process"""

    for pos in ["|", "/", "-", "\\"]:
        if lock:
            lock.acquire()

        try:
            sys.stdout.write(f"processing... [{pos}]\r")
            sys.stdout.flush()
        finally:
            if lock:
                lock.release()

        time.sleep(0.1)
