import pytest
from mtui import repoparse
from mtui.types import Product
from pathlib import Path

def test_parse_product():
    """
    Test _parse_product
    """
    products = repoparse._parse_product("SLES 15 (x86_64, aarch64)")
    assert Product("SLES", "15", "x86_64") in products
    assert Product("SLES", "15", "aarch64") in products

def test_slrepoparse():
    """
    Test slrepoparse
    """
    repos = repoparse.slrepoparse("https://example.com", ["SLES 15 (x86_64)"])
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/images/repo/SLES-15-x86_64/"

def test_gitrepoparse():
    """
    Test gitrepoparse
    """
    repos = repoparse.gitrepoparse("https://example.com", ["SLES 15 (x86_64)"])
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/standard"

def test_reporepoparse():
    """
    Test reporepoparse
    """
    repos = repoparse.reporepoparse(
        frozenset(["https://example.com/SLES-15-x86_64/"]), ["SLES 15 (x86_64)"]
    )
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/SLES-15-x86_64/"

def test_obsrepoparse(tmpdir):
    """
    Test obsrepoparse
    """
    project_xml = """
    <project>
      <repository name="SLE-15-x86_64">
        <path repository="update" project="SUSE:SLE-15:Update"/>
        <releasetarget project="SLE-Product-SLES:15:x86_64"/>
      </repository>
    </project>
    """
    path = Path(tmpdir)
    path.joinpath("project.xml").write_text(project_xml)

    repos = repoparse.obsrepoparse("https://example.com", path)
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/SLE-15-x86_64"
