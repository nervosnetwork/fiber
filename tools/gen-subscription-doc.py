#!/usr/bin/env python3
"""Include streaming subscription RPCs in fiber-rpc-gen's output."""

from pathlib import Path
import re


def generate(rpc_dir):
    source = (rpc_dir / "subscription.rs").read_text(encoding="utf-8")
    match = re.search(
        r"(?m)^((?:///.*\n)+)#\[rpc\(server\)\]\npub\(crate\) trait SubscriptionRpc\b", source
    )
    if match is None:
        raise ValueError("SubscriptionRpc must have a Rustdoc API reference")
    docs = "\n".join(
        line[4:] if line.startswith("/// ") else line[3:]
        for line in match[1].splitlines()
    ) + "\n"

    # Derive the TOC from the same headings as the generated method references.
    toc = []
    module = None
    for heading in re.finditer(r"(?m)^(### Module|#### Method) `([\w]+)`$", docs):
        kind, name = heading.groups()
        if kind == "### Module":
            module = name.lower()
            toc.append(f"    * [Module {name}](#module-{module})\n")
        elif module is not None:
            toc.append(f"        * [Method `{name}`](#{module}-{name})\n")
    if module != "subscription" or len(toc) != 3:
        raise ValueError("expected Subscription module with subscribe/unsubscribe headings")

    readme_path = rpc_dir / "README.md"
    readme = readme_path.read_text(encoding="utf-8")
    if '<a id="subscription"></a>' in readme:
        raise ValueError("run fiber-rpc-gen before adding subscription documentation")
    for anchor, addition in [
        ("    * [Module Watchtower](#module-watchtower)\n", "".join(toc)),
        ('<a id="watchtower"></a>\n', docs + "\n\n\n"),
    ]:
        if readme.count(anchor) != 1:
            raise ValueError(f"expected exactly one RPC documentation anchor: {anchor!r}")
        readme = readme.replace(anchor, addition + anchor)
    readme_path.write_text(readme, encoding="utf-8")


if __name__ == "__main__":
    generate(Path(__file__).resolve().parents[1] / "crates/fiber-lib/src/rpc")
