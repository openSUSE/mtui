class EmptyCheck:
    def _check(self, target, stdin, stdout, stderr, exitcode: int) -> None:
        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode: int) -> None:
        pass
