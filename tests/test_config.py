import pytest
from mtui import config
from pathlib import Path
from argparse import Namespace

class MockRefhosts:
    def __init__(self, config):
        pass
    def check_location_sanity(self, location):
        pass
    def __call__(self, config):
        return self

def test_default_config(tmpdir):
    """
    Test default config
    """
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.location == "default"
    assert cfg.datadir == Path("/usr/share/mtui")
    assert cfg.connection_timeout == 300

def test_override_config(tmpdir):
    """
    Test override config
    """
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text(
        "[mtui]\n"
        "location = test_location\n"
        "datadir = /test/datadir\n"
        "connection_timeout = 600\n"
    )
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.location == "test_location"
    assert cfg.datadir == Path("/test/datadir")
    assert cfg.connection_timeout == 600

def test_merge_args(tmpdir):
    """
    Test merge_args
    """
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    args = Namespace(
        location="cmd_location",
        template_dir="/cmd/template_dir",
        connection_timeout=1200,
        smelt_api="https://cmd/smelt_api",
        gitea_token="cmd_gitea_token",
    )
    cfg.merge_args(args)
    assert cfg.location == "cmd_location"
    assert cfg.template_dir == "/cmd/template_dir"
    assert cfg.connection_timeout == 1200
    assert cfg.smelt_api == "https://cmd/smelt_api"
    assert cfg.gitea_token == "cmd_gitea_token"
