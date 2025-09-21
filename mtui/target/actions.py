"""Classes for performing actions on groups of target hosts in parallel."""

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
    """A thread that executes a method from a queue."""

    def __init__(self, queue: Queue[tuple[Callable[..., None], list[Any]]]) -> None:
        """Initializes the thread.

        Args:
            queue: The queue to get methods from.
        """
        threading.Thread.__init__(self)
        self.queue = queue

    def run(self) -> None:
        """Runs the thread."""
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
    """An abstract base class for performing actions on a group of targets."""

    def __init__(self, targets: list[Target] | ValuesView[Target]) -> None:
        """Initializes the threaded target group.

        Args:
            targets: A list of targets to perform actions on.
        """
        self.targets = targets

    def mk_thread(self) -> None:
        """Creates and starts a new thread."""
        thread = ThreadedMethod(queue)
        thread.daemon = True
        thread.start()

    def mk_threads(self) -> None:
        """Creates and starts a thread for each target."""
        for _ in range(0, len(self.targets)):
            self.mk_thread()

    def run(self) -> None:
        """Runs the action on all targets."""
        self.mk_threads()
        self.setup_queue()

        while queue.unfinished_tasks:
            spinner()

        queue.join()

    @abstractmethod
    def mk_cmd(self, *args, **kwds) -> tuple[Callable[..., None], list[Any]]:
        """An abstract method for creating a command to be executed."""
        pass

    def setup_queue(self) -> None:
        """Sets up the queue with commands to be executed."""
        for t in self.targets:
            queue.put(self.mk_cmd(t))


class FileDelete(ThreadedTargetGroup):
    """Deletes a file on a group of targets in parallel."""

    def __init__(self, targets: list[Target] | ValuesView[Target], path: Path) -> None:
        """Initializes the file delete action.

        Args:
            targets: A list of targets to delete the file from.
            path: The path to the file to delete.
        """
        super().__init__(targets)
        self.path = path

    def mk_cmd(self, t: Target):
        """Creates a command to delete a file on a target.

        Args:
            t: The target to delete the file from.

        Returns:
            A tuple containing the method to execute and its arguments.
        """
        return (t.sftp_remove, [self.path])


class FileUpload(ThreadedTargetGroup):
    """Uploads a file to a group of targets in parallel."""

    def __init__(
        self, targets: list[Target] | ValuesView[Target], local: Path, remote: Path
    ) -> None:
        """Initializes the file upload action.

        Args:
            targets: A list of targets to upload the file to.
            local: The local path to the file to upload.
            remote: The remote path to upload the file to.
        """
        super().__init__(targets)
        self.local = local
        self.remote = remote

    def mk_cmd(self, t: Target):
        """Creates a command to upload a file to a target.

        Args:
            t: The target to upload the file to.

        Returns:
            A tuple containing the method to execute and its arguments.
        """
        return (t.sftp_put, [self.local, self.remote])


class FileDownload(ThreadedTargetGroup):
    """Downloads a file from a group of targets in parallel."""

    def __init__(
        self, targets: list[Target] | ValuesView[Target], remote: Path, local: Path
    ) -> None:
        """Initializes the file download action.

        Args:
            targets: A list of targets to download the file from.
            remote: The remote path to the file to download.
            local: The local path to save the downloaded file to.
        """
        super().__init__(targets)

        self.remote = remote
        self.local = local

    def mk_cmd(self, t: Target):
        """Creates a command to download a file from a target.

        Args:
            t: The target to download the file from.

        Returns:
            A tuple containing the method to execute and its arguments.
        """
        return (t.sftp_get, [self.remote, self.local])


class RunCommand:
    """Runs a command on a group of targets in parallel or serial."""

    def __init__(
        self, targets: dict[str, Target], command: str | dict[str, Any]
    ) -> None:
        """Initializes the run command action.

        Args:
            targets: A dictionary of targets to run the command on.
            command: The command to run.
        """
        self.targets = targets
        self.command = command

    def run(self) -> None:
        """Runs the command on all targets."""
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
    """A simple spinner to show that a process is running.

    Args:
        lock: An optional lock to use when printing to the console.
    """

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
