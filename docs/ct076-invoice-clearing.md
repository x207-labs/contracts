# CT-076: invoice clearing (design)

Design note. No change to the contract. It reads the registration code of `voucher.rs` (interface 0.0.7) to show that an invoice can be
imported without a new edge type and without touching the clearing invariants.

## The idea
A seller (the *issuer*) holds a claim on a buyer (the *debtor*) outside x207: an invoice. If the debtor signs it as a voucher, the claim becomes
an ordinary D2 edge `debtor -> issuer` and takes part in F3 clearing with everything else.

## Schema
```
Invoice {
  issuer: AccountId      // creditor, payee of the voucher
  debtor: AccountId      // payer of the voucher
  amount: U128           // base units of credit
  due: epoch id          // the epoch whose cutoff the invoice must be registered before
  invoice_hash: hex64    // sha256 of the canonical invoice document, kept off chain
  signature: ed25519     // of the debtor, over the voucher below
}
```
The signed object is the **voucher** that exists today: `payer = debtor`, `payee = issuer`, `epoch = due`, `unit = "credit"`, `resource =
"x207:invoice:<invoice_hash>"` (ASCII, accepted by `ERR_RESOURCE_NOT_ASCII`), and `cumulativeAmount = previous cumulative of the pair + amount`.
The `seq` is the next `seq` of the pair. Nothing is added to the contract: the issuer (or anybody) calls `register_cumulative(voucher, signature)`.

## Where the debtor signature comes from
The debtor's voucher key (`pubkey_voucher` of D1), exactly as for any payment. An invoice nobody signed cannot be imported. The debtor signs after
checking the invoice document against `invoice_hash`. An agent policy (`maxPerCall`, `maxPerEpoch`) still applies on the debtor's side.

## Why the clearing invariants hold
All checks of `register_with` run on the invoice voucher: circle, unit, ASCII resource, payer != payee, epoch open and before cutoff, both members
`active`, not exiting, `seq` increasing, `cumulative` not decreasing, `delta <= headroom` (limit minus debt minus headroom reserved for bundle legs),
and the debtor signature. Because the delta is booked in `outstanding_debt` of the debtor and in the totals of both sides, `sum(due) = sum(claimable)`
(CT-028) is untouched, and the credit limit bounds what invoices can add. An invoice for a debtor without headroom is refused `ERR_INSUFFICIENT_CREDIT`.

## Rules to add around it (no contract change)
- `due` is an epoch, not a date. The issuer must register before the cutoff of that epoch; the sdk can map a date to the epoch that is open then.
- Partial payment of an invoice by other means (an `exact` payment, or a payment in a previous epoch) is the debtor's business: a voucher carries
  an amount, so the debtor signs the amount still open.
- One `invoice_hash` is used once: the backend keeps a set per circle, since the contract does not look inside `resource`.
- Disputes stay off chain; x207 does not judge the quality of what was invoiced (same rule as F8).

## Open points
Whether the read model should show invoices (`resource` prefix `x207:invoice:` in `voucher_registered` events does not carry the resource today);
whether a contract-level `import_invoice` is worth having once more than a few invoices a day exist, because it could check `invoice_hash` uniqueness
on chain.
