"""Parsers for extracting metadata from text."""

import re
from typing import final


@final
class ReducedMetadataParser:
    """A parser for extracting a reduced set of metadata from text."""

    hostnames = re.compile(r".* \(reference host: (\S+).*\)")
    jira = re.compile(r'Jira ([A-Z]+-\d+) \("(.*)"\):')
    bugs = re.compile(r'Bug (\d+) \("(.*)"\):')

    @classmethod
    def parse(cls, results, line: str) -> None:
        """Parses a line of text and extracts metadata.

        Args:
            results: An object to store the parsed results.
            line: The line of text to parse.
        """
        if match := re.search(cls.hostnames, line):
            if "?" not in match.group(1):
                results.hostnames.add(match.group(1))
                return

        if match := re.search(cls.jira, line):
            results.jira[match.group(1)] = match.group(2)
            return

        if match := re.search(cls.bugs, line):
            results.bugs[match.group(1)] = match.group(2)
