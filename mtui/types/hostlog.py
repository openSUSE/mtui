from . import CommandLog


def to_string(item: str | bytes) -> str:
    if isinstance(item, bytes):
        return item.decode()
    else:
        return item


class HostLog(list):
    log = CommandLog

    def __init__(self) -> None:
        super().__init__()

    def append(self, *args) -> None:
        # there is awfull exceptation *args will expand into one  variable
        if len(args) == 1 and isinstance(*args, list | tuple | set):
            if len(*args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            items = args[0]
            command = to_string(items[0])
            stdout = to_string(items[1])
            stderr = to_string(items[2])
            exitcode = int(items[3])
            runtime = int(items[4])
        else:
            if len(args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            command = to_string(args[0])
            stdout = to_string(args[1])
            stderr = to_string(args[2])
            exitcode = int(args[3])
            runtime = int(args[4])

        super().append(
            self.log(
                command,
                stdout,
                stderr,
                exitcode,
                runtime,
            )
        )

    # suprisingly this isn't used ?
    def insert(self, pos, *args) -> None:
        if len(args) == 1 and isinstance(*args, list | tuple | set):
            if len(*args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            command = to_string(*args[0])
            stdout = to_string(*args[1])
            stderr = to_string(*args[2])
            exitcode = int(*args[3])
            runtime = int(*args[4])
        else:
            if len(args) != 5:
                raise ValueError(f"it need 5 args, got {len(*args)}")
            command = to_string(args[0])
            stdout = to_string(args[1])
            stderr = to_string(args[2])
            exitcode = int(args[3])
            runtime = int(args[4])
        super().insert(
            pos,
            self.log(
                command,
                stdout,
                stderr,
                exitcode,
                runtime,
            ),
        )
