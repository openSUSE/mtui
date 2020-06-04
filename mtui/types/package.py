
class Package:
    __slots__ = ["name", "before", "after", "required", "current"]

    def __init__(self, name):
        self.name = name
        self.before = None
        self.after = None
        self.required = None
        self.current = None

    def set_versions(self, before=None, after=None, required=None):
        if before:
            self.before = before
        if after:
            self.after = after
        if required:
            self.required = required
