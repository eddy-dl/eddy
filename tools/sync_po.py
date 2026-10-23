"""Keeps a translation (.po) in step with the UI's @tr strings: lists the
strings it's missing, and with --prune drops the entries no longer used.
No gettext needed.

    python3 tools/sync_po.py lang/fr/LC_MESSAGES/eddy-ui.po [--prune]
    python3 tools/sync_po.py lang/fr/LC_MESSAGES/eddy-ui.po --missing-json

`--missing-json` prints the missing ones as JSON ([msgid, msgid_plural or
null]), for filling them in.
"""
import json
import pathlib
import re
import sys

STRING = r'"((?:[^"\\]|\\.)*)"'
# @tr("one" | "many" % n) or @tr("text", args...)
TR = re.compile(r'@tr\(\s*' + STRING + r'(?:\s*\|\s*' + STRING + r')?')


def ui_strings(root):
    found = {}
    for path in sorted(pathlib.Path(root).rglob("*.slint")):
        text = path.read_text(encoding="utf-8")
        for m in TR.finditer(text):
            found.setdefault(m.group(1), m.group(2))
    return found


def parse_po(path):
    """Entries as (msgid, block text), header first."""
    blocks = re.split(r"\n\s*\n", pathlib.Path(path).read_text(encoding="utf-8").strip())
    entries = []
    for block in blocks:
        m = re.search(r'^msgid "((?:[^"\\]|\\.)*)"', block, re.M)
        entries.append((m.group(1) if m else None, block))
    return entries


def main():
    po = sys.argv[1]
    flags = set(sys.argv[2:])
    strings = ui_strings("ui")
    entries = parse_po(po)
    have = {msgid for msgid, _ in entries if msgid}
    missing = [(s, p) for s, p in strings.items() if s not in have]
    unused = [msgid for msgid, _ in entries if msgid and msgid not in strings]
    if "--missing-json" in flags:
        print(json.dumps(missing, ensure_ascii=False, indent=1))
        return
    for s, p in missing:
        print("missing:", s if p is None else f"{s} | {p}")
    for s in unused:
        print("unused:", s)
    if "--prune" in flags and unused:
        kept = [block for msgid, block in entries if msgid is None or msgid in strings or msgid == ""]
        pathlib.Path(po).write_text("\n\n".join(kept) + "\n", encoding="utf-8")
        print(f"pruned {len(unused)}")
    print(f"{len(strings)} strings, {len(missing)} missing, {len(unused)} unused")


if __name__ == "__main__":
    main()
