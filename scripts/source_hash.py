#!/usr/bin/env python3
"""テンプレートの源泉のハッシュ(docs/simplify-and-assistant-plan.md §16.5)の参照実装。

Rust の `core::structure::source_hash` と同じ値を返す。試験 `tests/structure.rs` が 14 件で突き合わせる。

使い方:
  python3 scripts/source_hash.py <dictionary.json または templates14.json> [キーワード]
  → 1 行 1 件 "sha256:… <keywords[0]>"。キーワードを渡せばその 1 件だけ。

正規化: 本文は行の配列に揃え(文字列なら改行で割る)、各行の末尾の空白(半角空白・タブ・全角空白)を除き、
末尾の空行を除いて "\\n" で繋ぐ。モジュールは源泉の順に U+001E、name、"\\n"、text の各行(同じ規則)、"\\n"、
note(同じ規則)を続ける。keywords・extra(diagnosis など)・memo は含めない。NFC 正規化は行わない
(保存された文字のまま。Rust 側に crate を足さないため。§16.5 の注)。
"""
import hashlib
import json
import sys

TRAILING = " \t　"


def canonical_lines(text):
    if isinstance(text, list):
        text = "\n".join(text)
    lines = [line.rstrip("\r").rstrip(TRAILING) for line in text.split("\n")]
    while lines and lines[-1] == "":
        lines.pop()
    return lines


def canonical_source(template):
    parts = ["\n".join(canonical_lines(template.get("body", "")))]
    for module in template.get("modules", []) or []:
        if isinstance(module, str):
            module = {"name": module}  # 名前だけのモジュール(Rust の modules_field と同じく、文と使い方は空)
        if not isinstance(module, dict):
            continue
        parts.append("\u001e")
        parts.append(module.get("name", "").rstrip(TRAILING))
        parts.append("\n")
        parts.append("\n".join(canonical_lines(module.get("text", ""))))
        parts.append("\n")
        parts.append("\n".join(canonical_lines(module.get("note", ""))))
    return "".join(parts)


def source_hash(template):
    return "sha256:" + hashlib.sha256(canonical_source(template).encode("utf-8")).hexdigest()


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    with open(argv[1], encoding="utf-8") as f:
        data = json.load(f)
    templates = data.get("templates", data) if isinstance(data, dict) else data
    wanted = argv[2] if len(argv) > 2 else None
    for template in templates:
        keywords = template.get("keywords", [])
        if isinstance(keywords, str):
            keywords = [k.strip() for k in keywords.replace("、", ",").split(",") if k.strip()]
        head = keywords[0] if keywords else ""
        if wanted is not None and wanted not in keywords:
            continue
        print(source_hash(template), head)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
