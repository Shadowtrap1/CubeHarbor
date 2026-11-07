#!/usr/bin/env python3
"""Regenerate the values table in charts/cubeharbor/README.md from values.yaml.

Each leaf value becomes a row: its dotted key, its default, and the `# --` comment above it (or
above the closest documented parent). The table sits between the `<!-- values:start -->` and
`<!-- values:end -->` markers; the rest of the README is left alone.

    scripts/chart-readme.py           rewrite the table
    scripts/chart-readme.py --check   exit 1 if the table is out of date (for CI)

No dependencies beyond the Python standard library: values.yaml is read line by line, which
works because it only uses block mappings, block lists of scalars and inline `{}`/`[]` values.
"""
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
VALUES = os.path.join(ROOT, "charts/cubeharbor/values.yaml")
README = os.path.join(ROOT, "charts/cubeharbor/README.md")
START, END = "<!-- values:start -->", "<!-- values:end -->"

KEY = re.compile(r"^(\s*)([A-Za-z0-9_.-]+):(?:\s+(.*))?$")


def parse(lines):
    """(path, default, description) for every leaf, and for every documented map, in file order."""
    rows = []
    stack = []  # (indent, key, description)
    pending = []  # comment lines of the current `# --` block
    i = 0
    while i < len(lines):
        line = lines[i].rstrip("\n")
        stripped = line.strip()
        i += 1
        if stripped.startswith("#"):
            # `# --` starts a description; plain comment lines continue it. Commented-out YAML
            # (examples such as `#  - name: games` or `#   tls: [...]`) is skipped.
            text = stripped[1:].strip()
            if text.startswith("--"):
                pending = [text[2:].strip()]
            elif text and not re.match(r"^[A-Za-z0-9_./-]+:(\s|$)", text) and not text.startswith("- "):
                pending.append(text)
            continue
        if not stripped:
            pending = []
            continue
        m = KEY.match(line)
        if not m:
            continue
        indent, key, value = len(m.group(1)), m.group(2), (m.group(3) or "")
        value = re.sub(r"\s+#.*$", "", value).strip()
        desc = " ".join(pending).strip()
        pending = []
        while stack and stack[-1][0] >= indent:
            stack.pop()
        path = ".".join([s[1] for s in stack] + [key])
        if value:
            rows.append((path, value, desc))
            continue
        # No inline value: a nested map, or a block list of scalars.
        j = i
        while j < len(lines) and (not lines[j].strip() or lines[j].strip().startswith("#")):
            j += 1
        nxt = lines[j] if j < len(lines) else ""
        if nxt.strip().startswith("- "):
            items = []
            while i < len(lines) and (lines[i].strip().startswith("- ") or not lines[i].strip()
                                      or lines[i].strip().startswith("#")):
                if lines[i].strip().startswith("- "):
                    items.append(lines[i].strip()[2:].strip())
                i += 1
            rows.append((path, "[" + ", ".join(items) + "]", desc))
        elif len(nxt) - len(nxt.lstrip()) > indent:
            stack.append((indent, key, desc))
            if desc:
                rows.append((path, "", desc))
        else:
            rows.append((path, "null", desc))
    return rows


def cell(text):
    return text.replace("|", "\\|")


def table(rows):
    out = ["| Key | Default | Description |", "|---|---|---|"]
    for path, default, desc in rows:
        shown = f"`{cell(default)}`" if default else ""
        out.append(f"| `{path}` | {shown} | {cell(desc)} |")
    return "\n".join(out)


def main():
    with open(VALUES) as f:
        rows = parse(f.readlines())
    with open(README) as f:
        readme = f.read()
    if START not in readme or END not in readme:
        sys.exit(f"{README} has no {START} ... {END} markers")
    head, rest = readme.split(START, 1)
    _, tail = rest.split(END, 1)
    new = f"{head}{START}\n{table(rows)}\n{END}{tail}"
    if "--check" in sys.argv:
        if new != readme:
            sys.exit("charts/cubeharbor/README.md values table is out of date: run make chart-readme")
        return
    with open(README, "w") as f:
        f.write(new)


if __name__ == "__main__":
    main()
