"""Heuristics shared with upstream oqa-search.

Keep verbatim to avoid behavioural drift when comparing output against
the upstream tool.
"""

import re

MICRO_TEMPLATE_IDENTIFIER = "sle-micro"

EXCLUDED_GROUPS: list[str] = [
    "DEV",
    "Leap",
    "Development",
    "Micro",
    "Kernel",
    "Wicked",
]

SINGLE_INCIDENTS_TERMS: list[str] = ["Core Incidents", "Core Staging"]

AGGREGATED_GROUPS_TERMS: list[str] = ["Maintenance Updates"]

AGGREGATED_NAME_MAP: dict[str, str] = {"Public Cloud": "cloud", "SAP/HA": "sap"}

AGGREGATED_EXCLUDED_VERSIONS: list[str] = ["TERADATA", "16.0"]

OQA_QUERY_STRINGS: dict[str, str] = {
    "failed": "&result=failed&result=incomplete&result=timeout_exceeded",
    "running": "&state=scheduled&state=running",
    "all": "",
}

TESTSUITE_NUMBERS_PATTERN = re.compile(r"(?:^|\s|\()\d+(?=$|\s|\))")

TESTSUITE_WORDS_BLOCKLIST: list[str] = [
    "syntax",
    "--",
    "meson",
    "gcc",
    "clang",
    "make",
    "cmake",
    "/usr/bin",
    ".tap",
    ".sh",
    "t/",
    "TODO",
    " - ",
    "duration",
    " + ",
    "group",
    "value",
    "doc",
    "stack",
    "errno",
    "tests in",
    "limit",
    "size",
    "test for",
    "creating",
    "task",
    "no tests",
    "thread",
    "server",
    "method",
    "object",
    "issue",
    "line",
    "set",
    "test_",
    "example",
    "flag",
    "print",
    "extra",
]

TESTSUITE_VISUAL_SEPARATORS: list[str] = ["===", "---"]

TESTSUITE_SUMMARY_KEYWORDS: list[str] = [
    "result:",
    "summary",
    "out of",
    "tests passed",
    "tests failed",
]

TESTSUITE_SUMMARY_PATTERNS: list[re.Pattern[str]] = [
    re.compile(r"\bok\s*\("),
    re.compile(r"\d+%\s+tests?\s+passed"),
    re.compile(r"\d+\s+tests?\s+(ok|passed|failed|skipped)"),
    re.compile(r"#\s*(total|pass|fail|skip|xfail|xpass|error):"),
    re.compile(r"^(ok|fail|expected fail|unexpected pass|skipped):\s*\d+"),
]

PYTHON_FLAVOR_RE = re.compile(r"^python\d+-")
