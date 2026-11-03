#!/usr/bin/env python3
"""CT-099: the golden vector of `edges_hash` and `positions_hash` (interface 0.0.13).

    python3 -I scripts/clearing_hashes.py           # writes tests/vectors/clearing_hashes.json
    python3 -I scripts/clearing_hashes.py --check   # recomputes and compares with the file; exit 1 on a difference

Definitions (the contract computes the same bytes in `events.rs`, a Rust test compares them with this file):
  edges_hash     = sha256( compact JSON array of the edges with `cleared` > 0, sorted by (debtor, creditor) as byte strings, each edge
                   {"debtor":..,"creditor":..,"cleared":"<u128 as decimal string>"} )
  positions_hash = sha256( compact JSON array of all positions sorted by account_id as byte strings, each
                   {"account_id":..,"net":"<i128>","role":..,"status":<string or null>,"amount":"<u128>","outstanding_debt":"<u128>"} )
Compact JSON: no spaces, keys in the order above, UTF-8. An empty list is `[]`.
"""
import hashlib
import json
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tests" / "vectors" / "clearing_hashes.json"
A, B, C, D = (f"{n}.x207-circle.near" for n in ("andi", "budi", "citra", "dewi"))
LONG = lambda i: f"m{i:02}" + "x" * (64 - len(".test.near") - 3) + ".test.near"


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def edges_canonical(edges):
    kept = [e for e in edges if int(e["cleared"]) > 0]
    kept.sort(key=lambda e: (e["debtor"].encode(), e["creditor"].encode()))
    return compact([{"debtor": e["debtor"], "creditor": e["creditor"], "cleared": str(int(e["cleared"]))} for e in kept])


def positions_canonical(positions):
    ordered = sorted(positions, key=lambda p: p["account_id"].encode())
    return compact(
        [
            {"account_id": p["account_id"], "net": str(int(p["net"])), "role": p["role"], "status": p["status"], "amount": str(int(p["amount"])), "outstanding_debt": str(int(p["outstanding_debt"]))}
            for p in ordered
        ]
    )


sha = lambda text: hashlib.sha256(text.encode()).hexdigest()

CASES = {
    "edges": [
        {
            "name": "three cycle, given out of order, one edge with cleared 0 that is dropped",
            "edges": [
                {"debtor": C, "creditor": A, "cleared": "25000000"},
                {"debtor": A, "creditor": B, "cleared": "25000000"},
                {"debtor": D, "creditor": A, "cleared": "0"},
                {"debtor": B, "creditor": C, "cleared": "25000000"},
            ],
        },
        {"name": "no edge", "edges": []},
        {
            "name": "names of 64 characters and the largest amount of a u128",
            "edges": [
                {"debtor": LONG(1), "creditor": LONG(0), "cleared": "340282366920938463463374607431768211455"},
                {"debtor": LONG(0), "creditor": LONG(1), "cleared": "1"},
            ],
        },
    ],
    "positions": [
        {
            "name": "a debtor, a creditor, a net zero participant (role none, status null), given out of order",
            "positions": [
                {"account_id": C, "net": "0", "role": "none", "status": None, "amount": "0", "outstanding_debt": "0"},
                {"account_id": B, "net": "35000000", "role": "creditor", "status": "claimable", "amount": "35000000", "outstanding_debt": "0"},
                {"account_id": A, "net": "-35000000", "role": "debtor", "status": "due", "amount": "35000000", "outstanding_debt": "35000000"},
            ],
        },
        {"name": "no position", "positions": []},
    ],
}


def build():
    out = {"definition": __doc__.strip().split("\n\n", 1)[1].strip(), "edges": [], "positions": []}
    for case in CASES["edges"]:
        canonical = edges_canonical(case["edges"])
        out["edges"].append({"name": case["name"], "input": case["edges"], "canonical": canonical, "edges_hash": sha(canonical)})
    for case in CASES["positions"]:
        canonical = positions_canonical(case["positions"])
        out["positions"].append({"name": case["name"], "input": case["positions"], "canonical": canonical, "positions_hash": sha(canonical)})
    return out


if __name__ == "__main__":
    built = build()
    if "--check" in sys.argv:
        on_disk = json.loads(OUT.read_text())
        if on_disk != built:
            print("clearing_hashes.json differs from the computed vector", file=sys.stderr)
            sys.exit(1)
        print("ok: tests/vectors/clearing_hashes.json matches the definition")
    else:
        OUT.write_text(json.dumps(built, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {OUT}")
