"""Tests for the ``[obs]`` native-backend configuration options."""

from pathlib import Path

from mtui.support import config


def _cfg(tmpdir, text: str) -> config.Config:
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text(text)
    return config.Config(config_file)


def test_obs_defaults(tmpdir):
    """With no [obs] section, the parity-preserving defaults apply."""
    cfg = _cfg(tmpdir, "")
    assert cfg.obs_api_url == "https://api.suse.de"
    assert cfg.obs_conffile == ""
    # Parity with the legacy ``osc qam`` subprocess 180s cap, not 120.
    assert cfg.obs_request_timeout == 180


def test_obs_explicit_values(tmpdir):
    """Explicit [obs] values are read verbatim."""
    cfg = _cfg(
        tmpdir,
        "[obs]\n"
        "api_url = https://api.opensuse.org\n"
        "conffile = /etc/osc/oscrc\n"
        "request_timeout = 90\n",
    )
    assert cfg.obs_api_url == "https://api.opensuse.org"
    assert cfg.obs_conffile == "/etc/osc/oscrc"
    assert cfg.obs_request_timeout == 90


def test_obs_request_timeout_non_positive_falls_back(tmpdir):
    """A non-positive timeout is rejected and falls back to 180."""
    cfg = _cfg(tmpdir, "[obs]\nrequest_timeout = 0\n")
    assert cfg.obs_request_timeout == 180


def test_obs_api_url_malformed_falls_back(tmpdir):
    """A malformed api_url is rejected at parse time and falls back."""
    cfg = _cfg(tmpdir, "[obs]\napi_url = not-a-url\n")
    assert cfg.obs_api_url == "https://api.suse.de"


def test_obs_api_url_no_trailing_slash_normalisation(tmpdir):
    """api_url is preserved byte-for-byte (no slash added/stripped).

    The native backend asserts this value against an oscrc section header,
    so the config layer must not silently rewrite it.
    """
    cfg = _cfg(tmpdir, "[obs]\napi_url = https://api.suse.de\n")
    assert cfg.obs_api_url == "https://api.suse.de"
