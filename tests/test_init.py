from mtui.config import Config
from mtui.systemcheck import detect_system


def test_systemcheck():
    config = Config([])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    config.distro, config.distro_ver, config.distro_kernel = detect_system()
    assert config.distro
    assert config.distro_ver
    assert config.distro_kernel
