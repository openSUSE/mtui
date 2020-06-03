from collections import namedtuple


def to_string(item):
    if isinstance(item, bytes):
        return item.decode()
    else:
        return item


class HostLog(list):
    log = namedtuple(
        "CommandLog", ["command", "stdout", "stderr", "exitcode", "runtime"]
    )

    def __init__(self):
        super().__init__()

    def append(self, *args):
        if isinstance(*args, (list, tuple, set)):
            if len(*args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            items = args[0]
            command = to_string(items[0])
            stdout = to_string(items[1])
            stderr = to_string(items[2])
            exitcode = int(items[3])
            runtime = int(items[4])
        else:
            command = to_string(args[0])
            stdout = to_string(args[1])
            stderr = to_string(args[2])
            exitcode = int(args[3])
            runtime = int(args[4])
        super().append(
            self.log(
                to_string(command),
                to_string(stdout),
                to_string(stderr),
                exitcode,
                runtime,
            )
        )

    def insert(self, pos, *args):
        if isinstance(*args, (list, tuple, set)):
            if len(*args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            command = to_string(*args[0])
            stdout = to_string(*args[1])
            stderr = to_string(*args[2])
            exitcode = int(*args[3])
            runtime = int(*args[4])
        else:
            command = to_string(args[0])
            stdout = to_string(args[1])
            stderr = to_string(args[2])
            exitcode = int(args[3])
            runtime = int(args[4])
        super().insert(
            pos,
            self.log(
                to_string(command),
                to_string(stdout),
                to_string(stderr),
                exitcode,
                runtime,
            ),
        )
