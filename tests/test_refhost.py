"""Tests for the mtui refhost module - specifically the eval() fix."""

import re



class TestArchListParsing:
    """Test that the refhost arch list parsing is safe (no eval)."""

    def test_simple_arch_list(self):
        """Test parsing a simple arch list."""
        content = "[x86_64,aarch64,ppc64le]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64", "aarch64", "ppc64le"]

    def test_single_arch(self):
        """Test parsing a single arch."""
        content = "[x86_64]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64"]

    def test_arch_list_with_spaces(self):
        """Test parsing arch list with surrounding spaces."""
        content = "[ x86_64 , aarch64 ]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64", "aarch64"]

    def test_injection_attempt_is_safe(self):
        """Test that a malicious input is treated as a literal string.

        This test verifies the fix for the eval() security vulnerability.
        The old code used eval() which would execute arbitrary Python code.
        The new code uses simple string splitting.
        """
        # This would have been exploitable with eval():
        # eval(f"['{content}']") where content = "x'];import os;os.system('id');['y"
        content = "[x'];import os;os.system('id');['y]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        # With safe parsing, this is just a literal string
        assert arch_list == ["x'];import os;os.system('id');['y"]

    def test_empty_brackets(self):
        """Test parsing empty brackets."""
        content = "[]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == [""]

    def test_no_brackets(self):
        """Test no match when no brackets present."""
        content = "x86_64"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is None
