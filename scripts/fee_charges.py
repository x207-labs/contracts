#!/usr/bin/env python3
"""CT-101: the golden vector of the clearing fees that `commit_epoch` takes from the stake of the debtors (interface 0.0.13).

    python3 -I scripts/fee_charges.py           # writes tests/vectors/fee_charges.json
    python3 -I scripts/fee_charges.py --check   # recomputes and compares with the file; exit 1 on a difference

The contract writes no event per member for this (the log of `commit_epoch` is full), so a reader that wants the stake of a member after a commit repeats the rule below.
Rule (`charge_fees` and `pro_rata_fee` in `commit.rs`), for a proposal with a solver, `cleared_total` = sum of `cleared` over the edges, and `fees` of the epoch:
  clearing_fee = cleared_total * clearing_fee_bps // 10000          solver_fee = cleared_total * solver_fee_bps // 10000
  nothing is charged when there is no solver, when cleared_total is 0 or when both bps are 0.
  fee_total = clearing_fee + solver_fee, split over the DEBTORS of the edges: debtor_cleared = sum of cleared of its edges;
    share = fee_total * debtor_cleared // cleared_total (floor); the units that are left (fee_total - sum of shares) go one by one, one unit each,
    to the debtors in account id order (byte order), the first ones first; debtors with share 0 are dropped.
  for each debtor in account id order: cut = min(share, stake); stake -= cut;
    limit = min(limit, max(stake - bond_locked, 0))      (`bound_limit`: a locked bond is not available as a limit)
  charged = sum of the cuts; to_reserve = min(clearing_fee, charged); to_solver = charged - to_reserve.
Amounts are decimal strings (u128).
"""
import json
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tests" / "vectors" / "fee_charges.json"
acct = lambda n: f"{n}.x207-circle.near"
CREDIT = 1_000_000


def charge(case):
    fees = case["fees"]
    edges = [(e["debtor"], e["creditor"], int(e["cleared"])) for e in case["edges"]]
    members = {m["account"]: {"stake": int(m["stake"]), "limit": int(m["limit"]), "bond_locked": int(m.get("bond_locked", "0"))} for m in case["members"]}
    cleared_total = sum(c for _, _, c in edges)
    out = {"cleared_total": str(cleared_total), "charges": [], "clearing_fee_charged": "0", "solver_fee_charged": "0"}
    if case["solver"] is None or cleared_total == 0 or (fees["clearing_fee_bps"] == 0 and fees["solver_fee_bps"] == 0):
        out["charges"] = [{"account": a, "stake_cut": "0", "stake_after": str(m["stake"]), "limit_after": str(m["limit"])} for a, m in sorted(members.items(), key=lambda kv: kv[0].encode())]
        return out
    clearing_fee = cleared_total * fees["clearing_fee_bps"] // 10000
    solver_fee = cleared_total * fees["solver_fee_bps"] // 10000
    fee_total = clearing_fee + solver_fee
    per_debtor = {}
    for d, _, c in edges:
        per_debtor[d] = per_debtor.get(d, 0) + c
    debtors = sorted(per_debtor, key=lambda a: a.encode())
    shares = {d: fee_total * per_debtor[d] // cleared_total for d in debtors}
    leftover = fee_total - sum(shares.values())
    for d in debtors:
        if leftover == 0:
            break
        shares[d] += 1
        leftover -= 1
    charged = 0
    result = {}
    for d in debtors:
        if shares[d] == 0:
            continue
        m = members[d]
        cut = min(shares[d], m["stake"])
        m["stake"] -= cut
        m["limit"] = min(m["limit"], max(m["stake"] - m["bond_locked"], 0))
        charged += cut
        result[d] = cut
    to_reserve = min(clearing_fee, charged)
    out["clearing_fee_charged"] = str(to_reserve)
    out["solver_fee_charged"] = str(charged - to_reserve)
    out["charges"] = [{"account": a, "stake_cut": str(result.get(a, 0)), "stake_after": str(m["stake"]), "limit_after": str(m["limit"])} for a, m in sorted(members.items(), key=lambda kv: kv[0].encode())]
    return out


def edge(d, c, cleared):
    return {"debtor": d, "creditor": c, "cleared": str(cleared)}


def member(a, stake, limit, bond="0"):
    return {"account": a, "stake": str(stake), "limit": str(limit), "bond_locked": str(bond)}


def cases():
    a, b, c, d = acct("andi"), acct("budi"), acct("citra"), acct("dewi")
    out = []
    # 1 the units that are left go one by one to the debtors in account order: cleared_total 30000001 gives fee_total 21000 whose floor shares leave units over
    out.append(dict(name="leftover units go one by one to the debtors in account order", solver=acct("solver"), fees={"clearing_fee_bps": 5, "solver_fee_bps": 2},
                    edges=[edge(a, b, 10_000_000), edge(b, c, 10_000_000), edge(c, a, 10_000_001)],
                    members=[member(a, 100 * CREDIT, 80 * CREDIT), member(b, 100 * CREDIT, 80 * CREDIT), member(c, 100 * CREDIT, 80 * CREDIT)]))
    # 2 a stake smaller than the share pays what it has; the limit goes to 0; less than the fee is collected
    out.append(dict(name="a stake smaller than its share pays what it has", solver=acct("solver"), fees={"clearing_fee_bps": 100, "solver_fee_bps": 50},
                    edges=[edge(a, b, 20 * CREDIT), edge(b, a, 20 * CREDIT)],
                    members=[member(a, 100_000, 80_000), member(b, 5 * CREDIT, 4 * CREDIT)]))
    # 3 a locked bond: the limit is stake - bond, and it falls with the stake
    out.append(dict(name="a locked bond keeps the limit at stake minus bond", solver=acct("solver"), fees={"clearing_fee_bps": 100, "solver_fee_bps": 100},
                    edges=[edge(a, b, 50 * CREDIT), edge(b, a, 50 * CREDIT)],
                    members=[member(a, 100 * CREDIT, 80 * CREDIT, 20 * CREDIT), member(b, 100 * CREDIT, 80 * CREDIT, 0)]))
    # 4 bound_limit lowers a limit that is above the stake after the cut
    out.append(dict(name="bound_limit lowers a limit above the new stake", solver=acct("solver"), fees={"clearing_fee_bps": 100, "solver_fee_bps": 0},
                    edges=[edge(a, b, 30 * CREDIT), edge(b, c, 30 * CREDIT), edge(c, a, 30 * CREDIT)],
                    members=[member(a, 100 * CREDIT, 100 * CREDIT), member(b, 100 * CREDIT, 99_950_000), member(c, 100 * CREDIT, 80 * CREDIT)]))
    # 5 fees of 0: nothing is charged
    out.append(dict(name="fees of zero charge nothing", solver=acct("solver"), fees={"clearing_fee_bps": 0, "solver_fee_bps": 0},
                    edges=[edge(a, b, 10 * CREDIT), edge(b, a, 10 * CREDIT)], members=[member(a, 50 * CREDIT, 40 * CREDIT), member(b, 50 * CREDIT, 40 * CREDIT)]))
    # 6 the testnet epoch 2 of CT-100: 15 members, a complete graph of 210 edges of 1 credit, fees 10 and 5 bps: fee_total 315000, 21000 from each
    names = [f"m{i:02}.x207-v013-aux.testnet" for i in range(15)]
    out.append(dict(name="testnet x207-v013 epoch 2: 15 members, 210 edges, a cut of 21000 each", solver="x207-v013.testnet", fees={"clearing_fee_bps": 10, "solver_fee_bps": 5},
                    edges=[edge(x, y, CREDIT) for x in names for y in names if x != y], members=[member(x, 100 * CREDIT, 80 * CREDIT) for x in names]))
    # 7 no solver (no proposal): nothing is charged
    out.append(dict(name="no solver, no proposal: nothing is charged", solver=None, fees={"clearing_fee_bps": 10, "solver_fee_bps": 5},
                    edges=[], members=[member(a, 50 * CREDIT, 40 * CREDIT), member(b, 50 * CREDIT, 40 * CREDIT)]))
    # 8 debtors with different cleared amounts: floor shares and a leftover unit on the first debtor
    out.append(dict(name="unequal cleared amounts: floor shares and one leftover unit", solver=acct("solver"), fees={"clearing_fee_bps": 7, "solver_fee_bps": 3},
                    edges=[edge(d, a, 3 * CREDIT), edge(a, b, 7 * CREDIT), edge(b, c, 5 * CREDIT), edge(c, d, 5 * CREDIT), edge(b, d, 2 * CREDIT), edge(a, c, 1 * CREDIT), edge(c, a, 1 * CREDIT),
                           edge(d, b, 2 * CREDIT), edge(c, b, 1)],
                    members=[member(a, 100 * CREDIT, 80 * CREDIT), member(b, 100 * CREDIT, 80 * CREDIT), member(c, 100 * CREDIT, 80 * CREDIT), member(d, 100 * CREDIT, 80 * CREDIT)]))
    return out


def build():
    result = {"definition": __doc__.strip().split("\n\n", 1)[1].strip(), "cases": []}
    for case in cases():
        result["cases"].append({**case, "expected": charge(case)})
    return result


if __name__ == "__main__":
    built = build()
    if "--check" in sys.argv:
        if json.loads(OUT.read_text()) != built:
            print("fee_charges.json differs from the computed vector", file=sys.stderr)
            sys.exit(1)
        print("ok: tests/vectors/fee_charges.json matches the rule")
    else:
        OUT.write_text(json.dumps(built, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {OUT}")
