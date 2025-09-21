"""A list-like object for storing command log entries."""

from . import CommandLog


def to_string(item: str | bytes) -> str:
    """Converts a string or bytes object to a string.

    Args:
        item: The string or bytes object to convert.

    Returns:
        The converted string.
    """
    if isinstance(item, bytes):
        return item.decode()
    else:
        return item


class HostLog(list):
    """A list-like object for storing command log entries."""

    log = CommandLog

    def __init__(self) -> None:
        """Initializes the `HostLog` object."""
        super().__init__()

    def append(self, *args) -> None:
        """Appends a command log entry to the list.

        Args:
            *args: The command log entry to append.
        """
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
        """Inserts a command log entry into the list.

        Args:
            pos: The position to insert the entry at.
            *args: The command log entry to insert.
        """
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
