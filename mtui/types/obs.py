# -*- coding: utf-8 -*-

from mtui.utils import check_eq
from argparse import ArgumentTypeError

from itertools import zip_longest


class RequestReviewIDParseError(ValueError, ArgumentTypeError):
    # Note: need to inherit ArgumentTypeError so the custom exception
    # messages get shown to the users properly
    # by L{argparse.ArgumentParser._get_value}

    def __init__(self, message):
        super(RequestReviewIDParseError, self).__init__(
            "OBS Request Review ID: " + message
        )


class TooManyComponentsError(RequestReviewIDParseError):
    limit = 4

    def __init__(self):
        super(TooManyComponentsError, self).__init__(
            "Too many components (> {0})".format(self.limit)
        )

    @classmethod
    def raise_if(cls, xs):
        if len(xs) > cls.limit:
            raise cls()


class InternalParseError(RequestReviewIDParseError):

    def __init__(self, f, cnt):
        super(InternalParseError, self).__init__(
            "Internal error: f: {0!r} cnt: {1!r}".format(f, cnt)
        )


class MissingComponent(RequestReviewIDParseError):

    def __init__(self, index, expected):
        super(MissingComponent, self).__init__(
            "Missing {0}. component. Expected: {1!r}".format(
                index, expected
                ))


class ComponentParseError(RequestReviewIDParseError):

    def __init__(self, index, expected, got):
        super(ComponentParseError, self).__init__(
            "Failed to parse {0}. component. Expected {1!r}. Got: {2!r}"
            .format(index, expected, got)
        )


class RequestReviewID(object):

    def __init__(self, rrid):
        """
        :type rrid: str
        :param rrid: fully qualified Request Review ID
        """
        parsers = [
            check_eq("SUSE", "openSUSE"), check_eq("Maintenance"), int, int
            ]

        # filter empty entries
        xs = [x for x in rrid.split(":") if x]
        TooManyComponentsError.raise_if(xs)
        # construct [(parser, input, index), ...]
        xs = zip_longest(parsers, xs, list(range(1, 5)))

        # apply parsers to inputs, getting parsed values or raise
        xs = [_apply_parser(*ys) for ys in xs]

        self.project, self.kind, self.maintenance_id, self.review_id = xs

    def __str__(self):
        return "{}:{}:{}:{}".format(
            self.project,
            self.kind,
            self.maintenance_id,
            self.review_id
        )

    def __hash__(self):
        return hash(str(self))

    def __eq__(lhs, rhs):
        return str(lhs) == str(rhs)

    def __ne__(lhs, rhs):
        return not lhs.__eq__(rhs)


def _apply_parser(f, x, cnt):
    if not f or not cnt:
        raise InternalParseError(f, cnt)

    if not x:
        raise MissingComponent(cnt, f)

    try:
        return f(x)
    except Exception as e:
        new = ComponentParseError(cnt, f, x)
        new.__cause__ = e
        raise new
