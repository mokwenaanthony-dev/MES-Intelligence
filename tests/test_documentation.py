"""The documentation rule (as in ERP-Intelligence, principle 16): every module explains itself and every
public class and function has a docstring. A new undocumented function fails CI."""
import ast
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
MODULES = sorted((ROOT / "src" / "mes").glob("*.py"))


def _undocumented(tree):
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.ClassDef)) and not node.name.startswith("_") \
                and ast.get_docstring(node) is None:
            yield f"{node.name} (line {node.lineno})"


@pytest.mark.parametrize("path", MODULES, ids=lambda p: p.name)
def test_documented(path):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    assert ast.get_docstring(tree), f"{path.name} has no module docstring"
    missing = list(_undocumented(tree))
    assert not missing, f"{path.name}: add docstrings to {', '.join(missing)}"


def test_the_scan_found_the_package():
    assert len(MODULES) >= 6
