from mtui.config import Config
from mtui.systemcheck import detect_system

import pytest


def test_systemcheck():
    config = Config([])
    config.distro, config.distro_ver, config.distro_kernel = detect_system()
    assert config.distro
    assert config.distro_ver
    assert config.distro_kernel
