import sys
import threading
import time
from queue import Queue

from mtui.utils import prompt_user

queue: 'Queue[int]' = Queue()


class UpdateError(Exception):
    def __init__(self, reason, host=None):
        self.reason = reason
        self.host = host

    def __str__(self):
        if self.host is None:
            string = self.reason
        else:
            string = "{!s}: {!s}".format(self.host, self.reason)
        return repr(string)


class ThreadedMethod(threading.Thread):
    def __init__(self, queue):
        threading.Thread.__init__(self)
        self.queue = queue

    def run(self):
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


class ThreadedTargetGroup(object):
    def __init__(self, targets):
        self.targets = targets

    def mk_thread(self):
        thread = ThreadedMethod(queue)
        thread.daemon = True
        thread.start()

    def mk_threads(self):
        for _ in range(0, len(self.targets)):
            self.mk_thread()

    def run(self):
        self.mk_threads()
        self.setup_queue()

        while queue.unfinished_tasks:
            spinner()

        queue.join()

    def setup_queue(self):
        for t in self.targets:
            queue.put(self.mk_cmd(t))


class FileDelete(ThreadedTargetGroup):
    def __init__(self, targets, path):
        super().__init__(targets)
        self.path = path

    def mk_cmd(self, t):
        return [t.remove, [self.path]]


class FileUpload(ThreadedTargetGroup):
    def __init__(self, targets, local, remote):
        super().__init__(targets)
        self.local = local
        self.remote = remote

    def mk_cmd(self, t):
        return [t.put, [self.local, self.remote]]


class FileDownload(ThreadedTargetGroup):
    def __init__(self, targets, remote, local):
        super().__init__(targets)

        self.remote = remote
        self.local = local

    def mk_cmd(self, t):
        return [t.get, [self.remote, self.local]]


class RunCommand:
    def __init__(self, targets, command):
        self.targets = targets
        self.command = command

    def run(self):
        parallel = {}
        serial = {}
        lock = threading.Lock()

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
                    queue.put([parallel[target].run, [self.command[target], lock]])
                elif isinstance(self.command, str):
                    queue.put([parallel[target].run, [self.command, lock]])

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
                queue.put([serial[target].run, [self.command, lock]])
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
                    thread.queue.task_done()
                except ValueError:
                    pass

            queue.join()
            print()
            raise


def spinner(lock=None):
    """simple spinner to show some process"""

    for pos in ["|", "/", "-", "\\"]:
        if lock is not None:
            lock.acquire()

        try:
            sys.stdout.write("processing... [{!s}]\r".format(pos))
            sys.stdout.flush()
        finally:
            if lock is not None:
                lock.release()

        time.sleep(0.3)
