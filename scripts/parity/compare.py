#!/usr/bin/env python3
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict


def outcomes(path):
    runs = defaultdict(list)
    for case in ET.parse(path).getroot().iter("testcase"):
        name = f'{case.get("classname")}.{case.get("name")}'
        if case.find("skipped") is not None:
            runs[name].append("skip")
        elif case.find("failure") is not None or case.find("error") is not None:
            runs[name].append("fail")
        else:
            runs[name].append("pass")
    return {name: final(results) for name, results in runs.items()}


def final(results):
    if "pass" in results:
        return "pass" if results.count("fail") == 0 else "flaky"
    if "fail" in results:
        return "fail"
    return "skip"


def summary(label, results):
    counts = defaultdict(int)
    for outcome in results.values():
        counts[outcome] += 1
    print(f"{label}: " + ", ".join(f"{key} {counts[key]}" for key in ("pass", "flaky", "fail", "skip")))


def main():
    go = outcomes(sys.argv[1])
    summary("go", go)
    if len(sys.argv) < 3:
        for name, outcome in sorted(go.items()):
            if outcome in ("fail", "flaky"):
                print(f"  {outcome:5} {name}")
        return
    rust = outcomes(sys.argv[2])
    summary("rust", rust)
    passed = {"pass", "flaky"}
    print("\nRust fails, Go passes:")
    for name in sorted(go):
        if go[name] in passed and rust.get(name, "missing") not in passed | {"skip"}:
            print(f"  {rust.get(name, 'missing'):7} {name}")
    print("\nGo fails, Rust passes:")
    for name in sorted(go):
        if go[name] == "fail" and rust.get(name) in passed:
            print(f"  {name}")
    print("\nBoth fail:")
    for name in sorted(go):
        if go[name] == "fail" and rust.get(name) == "fail":
            print(f"  {name}")


main()
