"""veil-api（Python バインディング）の Sphinx 設定。

Rust の拡張モジュールを読み込まず、sphinx-autoapi が ``../python/veil`` の ``.py`` と ``_veil.pyi`` を
静的に読んで API の一覧を作る（Read the Docs で Rust をコンパイルしなくてよい）。

日本語版（gettext）の作り方::

    pip install sphinx-intl
    sphinx-build -b gettext crates/veil-python/docs crates/veil-python/docs/_build/gettext
    sphinx-intl update -p crates/veil-python/docs/_build/gettext -l ja -d crates/veil-python/docs/locale
    # locale/ja/LC_MESSAGES/*.po を翻訳してから
    sphinx-build -b html -D language=ja crates/veil-python/docs crates/veil-python/docs/_build/html-ja

Read the Docs では、プロジェクトの翻訳（Translations）に ``ja`` のプロジェクトを追加して同じ設定でビルドする。
"""

import pathlib
import tomllib

_HERE = pathlib.Path(__file__).resolve().parent
# 版はワークスペースの [workspace.package] version（F-231）。crates/veil-python/Cargo.toml は workspace 継承
_ROOT_TOML = _HERE.parents[2] / "Cargo.toml"
release = tomllib.loads(_ROOT_TOML.read_text(encoding="utf-8"))["workspace"]["package"]["version"]
version = ".".join(release.split(".")[:2])

project = "veil-api"
author = "aofusa"
copyright = "aofusa"
language = "en"

extensions = [
    "myst_parser",
    "autoapi.extension",
    "sphinx.ext.napoleon",
]

source_suffix = {".md": "markdown", ".rst": "restructuredtext"}
root_doc = "index"
exclude_patterns = ["_build", "locale", "requirements.txt"]

myst_heading_anchors = 3

# --- 国際化（gettext） ---
locale_dirs = ["locale/"]
gettext_compact = False

# --- autoapi: ../python/veil を静的に読む（.pyi を含む） ---
autoapi_type = "python"
autoapi_dirs = ["../python", "../../veil-python-kit/src"]
autoapi_file_patterns = ["*.pyi", "*.py"]
autoapi_root = "api"
autoapi_add_toctree_entry = True
autoapi_keep_files = False
autoapi_python_class_content = "both"
autoapi_member_order = "bysource"
autoapi_options = [
    "members",
    "undoc-members",
    "private-members",  # veil._veil（先頭が _ のモジュール）を載せるため。他の _名は下で除く
    "show-inheritance",
    "show-module-summary",
]


def _skip_member(app, what, name, obj, skip, options):
    """veil._veil（拡張の型）は載せ、それ以外の先頭が ``_`` の内部名は載せない。"""
    last = name.rsplit(".", 1)[-1]
    if name == "veil._veil" or name.startswith("veil._veil."):
        return False
    if last.startswith("_") and not (last.startswith("__") and last.endswith("__")):
        return True
    return skip


def setup(sphinx):
    sphinx.connect("autoapi-skip-member", _skip_member)


# --- 見た目 ---
try:
    import furo  # noqa: F401

    html_theme = "furo"
except ImportError:
    html_theme = "alabaster"
html_title = "veil-api"
