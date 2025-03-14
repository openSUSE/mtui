class UpdateError(Exception):
    def __init__(self, reason: str, host: str | None = None) -> None:
        self.reason = reason
        self.host = host

    def __str__(self) -> str:
        if self.host is None:
            string = self.reason
        else:
            string = "{!s}: {!s}".format(self.host, self.reason)
        return repr(string)
