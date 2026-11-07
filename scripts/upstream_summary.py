"""Summaries for upstream (aws-cli) updates, used by scripts/upstream.sh.

    upstream_summary.py snapshot DATA_DIR OUT.json
        services -> latest API version and operation names
    upstream_summary.py report OLD.json NEW.json CHANGELOG.rst OLD_TAG NEW_TAG
        Markdown summary of what changed, for the update pull request
"""

import json
import os
import re
import sys

MAX_LISTED = 15


def snapshot(data_dir, out):
    services = {}
    for name in sorted(os.listdir(data_dir)):
        svc = os.path.join(data_dir, name)
        if name.startswith(".") or not os.path.isdir(svc):
            continue
        versions = sorted(v for v in os.listdir(svc) if os.path.isdir(os.path.join(svc, v)))
        if not versions:
            continue
        model = os.path.join(svc, versions[-1], "service-2.json")
        if not os.path.isfile(model):
            continue
        with open(model) as f:
            doc = json.load(f)
        services[name] = {
            "api_version": versions[-1],
            "protocol": doc.get("metadata", {}).get("protocol"),
            "operations": sorted(doc.get("operations", {})),
        }
    with open(out, "w") as f:
        json.dump(services, f, indent=1, sort_keys=True)


def changelog(path, old_tag, new_tag):
    """Changelog sections newer than old_tag, up to and including new_tag."""
    with open(path) as f:
        lines = f.read().splitlines()
    sections, current = [], None
    for i, line in enumerate(lines):
        nxt = lines[i + 1] if i + 1 < len(lines) else ""
        if re.fullmatch(r"\d+\.\d+\.\d+", line) and re.fullmatch(r"=+", nxt):
            current = [line, []]
            sections.append(current)
        elif current is not None and line.startswith("* "):
            current[1].append(line[2:])
    out, collecting = [], False
    for version, entries in sections:
        if version == new_tag:
            collecting = True
        if version == old_tag:
            break
        if collecting:
            out.append((version, entries))
    return out


def listed(names):
    shown = ", ".join(f"`{n}`" for n in names[:MAX_LISTED])
    if len(names) > MAX_LISTED:
        shown += f" and {len(names) - MAX_LISTED} more"
    return shown


def report(old_path, new_path, changelog_path, old_tag, new_tag):
    with open(old_path) as f:
        old = json.load(f)
    with open(new_path) as f:
        new = json.load(f)
    out = [f"## aws-cli {old_tag} → {new_tag}", ""]
    added = sorted(set(new) - set(old))
    removed = sorted(set(old) - set(new))
    total_ops = sum(len(s["operations"]) for s in new.values())
    out.append(f"{len(new)} services, {total_ops} operations.")
    out.append("")
    if added:
        out.append(f"- New services: {listed(added)}")
    if removed:
        out.append(f"- Removed services: {listed(removed)}")
    protocols = sorted(
        {s["protocol"] for s in new.values()} - {s["protocol"] for s in old.values()} - {None}
    )
    if protocols:
        out.append(f"- **New protocols (not supported by awsr yet?)**: {listed(protocols)}")
    rows = []
    for name in sorted(set(old) & set(new)):
        o, n = old[name], new[name]
        plus = sorted(set(n["operations"]) - set(o["operations"]))
        minus = sorted(set(o["operations"]) - set(n["operations"]))
        version = o["api_version"] != n["api_version"]
        if plus or minus or version:
            note = []
            if plus:
                note.append(f"+{listed(plus)}")
            if minus:
                note.append(f"-{listed(minus)}")
            if version:
                note.append(f"API version {o['api_version']} → {n['api_version']}")
            rows.append(f"| `{name}` | {'; '.join(note)} |")
    if rows:
        out += ["", "### Operation changes", "", "| Service | Change |", "|---|---|"] + rows
    entries = changelog(changelog_path, old_tag, new_tag)
    if entries:
        out += ["", "### CHANGELOG", ""]
        for version, items in entries:
            out.append(f"<details><summary>{version} ({len(items)} entries)</summary>")
            out.append("")
            out += [f"- {item}" for item in items]
            out += ["", "</details>"]
    print("\n".join(out))


if __name__ == "__main__":
    if sys.argv[1:2] == ["snapshot"] and len(sys.argv) == 4:
        snapshot(sys.argv[2], sys.argv[3])
    elif sys.argv[1:2] == ["report"] and len(sys.argv) == 7:
        report(*sys.argv[2:])
    else:
        sys.exit(__doc__)
