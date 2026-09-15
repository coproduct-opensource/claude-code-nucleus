#!/usr/bin/env python3
"""Derive the proxy's request shapes from its source and diff them against the contract.

Reads `nucleus-tool-proxy/src/main.rs`, finds the `#[derive(..., Deserialize, ...)]`
structs the contract names, and reports the wire field names each one accepts:

  * a field carrying `#[serde(default)]` is optional, anything else required;
  * `#[serde(rename = "x")]` decides the wire name, which is why `GrepRequest`'s
    `file_glob` is `glob` on the wire and this script cannot just read identifiers.

Exits non-zero, naming every difference, when the contract has drifted. The point
is that a rename in nucleus fails a build here rather than becoming a 422 in a
session.
"""

import json
import re
import sys

# A struct body, from `struct Name {` to the first line that is a bare `}`.
STRUCT = re.compile(
    r"^(?P<attrs>(?:#\[[^\n]*\]\s*\n)*)"
    r"(?:pub(?:\([^)]*\))?\s+)?struct\s+(?P<name>\w+)\s*\{(?P<body>.*?)^\}",
    re.MULTILINE | re.DOTALL,
)
RENAME = re.compile(r'rename\s*=\s*"([^"]+)"')
FIELD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?P<name>\w+)\s*:", re.MULTILINE)


def structs(source: str) -> dict[str, dict[str, list[str]]]:
    """Every Deserialize struct in `source`, as {name: {required: [...], optional: [...]}}."""
    out = {}
    for m in STRUCT.finditer(source):
        if "Deserialize" not in m.group("attrs"):
            continue
        required, optional = [], []
        # Split the body into fields: attributes attach to the field that follows.
        pending_attrs = ""
        for line in m.group("body").splitlines():
            stripped = line.strip()
            if stripped.startswith("//") or not stripped:
                continue
            if stripped.startswith("#["):
                pending_attrs += stripped
                continue
            f = FIELD.match(line)
            if not f:
                continue
            name = f.group("name")
            rename = RENAME.search(pending_attrs)
            if rename:
                name = rename.group(1)
            (optional if "default" in pending_attrs else required).append(name)
            pending_attrs = ""
        out[m.group("name")] = {"required": required, "optional": optional}
    return out


def main() -> int:
    source_file, contract_file = sys.argv[1], sys.argv[2]
    source = open(source_file, encoding="utf-8").read()
    contract = json.load(open(contract_file, encoding="utf-8"))
    found = structs(source)

    problems = []
    for route, spec in contract["routes"].items():
        name = spec["struct"]
        if name not in found:
            problems.append(
                f"{route}: `{name}` is not a Deserialize struct in {source_file} any more "
                f"— the route was renamed, removed, or its body restructured"
            )
            continue
        for kind in ("required", "optional"):
            want = sorted(spec.get(kind, []))
            got = sorted(found[name][kind])
            if want != got:
                problems.append(
                    f"{route} ({name}) {kind} fields drifted:\n"
                    f"    contract: {want}\n"
                    f"    nucleus:  {got}"
                )

    if problems:
        print("The proxy's request shapes no longer match contracts/tool-proxy-requests.json:\n")
        for p in problems:
            print(f"  - {p}")
        print(
            "\nUpdate the contract and `translate::to_proxy_body` together, then bump "
            "`_nucleus_commit`. Shipping the old names means every affected call 422s."
        )
        return 1

    print(
        f"OK: all {len(contract['routes'])} routes match their Deserialize structs "
        f"at {contract['_nucleus_commit']}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
