from itertools import zip_longest
from typing import Callable, final

from ..exceptions import TooManyComponentsError
from ..utils import apply_parser, check_eq, check_type


@final
class RequestReviewID:
    def __init__(self, rrid: str) -> None:
        """
        :type rrid: str
        :param rrid: fully qualified Request Review ID
        """
        xs: list[str] = [x for x in rrid.split(":") if x]
        parsers: list[Callable[[str], str | int]] = [
            check_eq("SUSE", "S"),
            check_eq("SLFO", "S", "Maintenance", "M", "PI", "P"),
            check_type(int, str),
            check_type(int),
        ]

        TooManyComponentsError.raise_if(xs, 4)

        xs = [
            apply_parser(*ys)
            for ys in zip_longest(parsers, xs, range(1, len(parsers) + 1))
        ]
        self.project, self.kind, self.maintenance_id, self.review_id = xs

        if self.project == "S":
            self.project = "SUSE"

        if self.kind == "M":
            self.kind = "Maintenance"
        elif self.kind == "S":
            self.kind = "SLFO"
        elif self.kind == "P":
            self.kind = "PI"

    def __str__(self) -> str:
        return f"{self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}"

    def __repr__(self) -> str:
        return f"<RRID - {self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}>"

    def __hash__(self) -> int:
        return hash(str(self))

    def __eq__(self, other: object) -> bool:
        return str(self) == str(other)

    def __ne__(self, other: object) -> bool:
        return not self.__eq__(other)
