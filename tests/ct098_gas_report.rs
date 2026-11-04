//! CT-098 in near-sandbox: gas and storage of the public methods of interface 0.0.12, for `docs/gas.md`.
//! Table: `cargo test --test ct098_gas_report -- --nocapture --test-threads=1 | grep '^GAS|'`. Needs `cargo near build non-reproducible-wasm` first.
//! Every line is `GAS|group|method (case)|Tgas|bytes of contract storage added by the call|note`. The gas is `total_gas_burnt` of the transaction, so it includes the callbacks
//! (`on_claim_resolved`, `on_escrow_resolved`, `on_withdraw_resolved`, ...) and the call of the token contract that the method makes. Views are free and are not measured here
//! (the long lists are in `ct086_gas_limits`). A call that is refused is printed as REFUSED with its reason, never as a number.

mod common;

use common::circle::{account_of, failure_text, CREDIT};
use common::flow::edge;
use common::{ft_escrow, mt_escrow};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use serde_json::{json, Value};

const LIMIT_TGAS: f64 = 300.0;

fn tgas(r: &ExecutionFinalResult) -> f64 {
    r.total_gas_burnt.as_gas() as f64 / 1e12
}

async fn storage(f: &Funded) -> anyhow::Result<i64> {
    Ok(f.circle.worker.view_account(f.circle.contract.id()).await?.storage_usage as i64)
}

/// Prints one line of the table. `before` is the storage before the call.
async fn line(f: &Funded, group: &str, label: &str, before: i64, r: &ExecutionFinalResult, note: &str) -> anyhow::Result<f64> {
    let after = storage(f).await?;
    match failure_text(r) {
        None => {
            let flag = if tgas(r) > LIMIT_TGAS { " ABOVE 300 Tgas" } else { "" };
            println!("GAS|{group}|{label}|{:.1}|{:+}|{note}{flag}", tgas(r), after - before);
        }
        Some(text) => println!("GAS|{group}|{label}|REFUSED after {:.1}|{:+}|{}", tgas(r), after - before, ["ExecutionError(\"", "ERR_"].iter().find_map(|k| text.find(k).map(|i| text[i..].chars().take(110).collect::<String>())).unwrap_or_else(|| text.chars().take(100).collect::<String>())),
    }
    Ok(tgas(r))
}

macro_rules! measure {
    ($f:expr, $group:expr, $label:expr, $note:expr, $call:expr) => {{
        let before = storage($f).await?;
        let r = $call.await?;
        line($f, $group, $label, before, &r, $note).await?;
        r
    }};
}

fn tx(who: &near_workspaces::Account, to: &str, method: &str, args: Value, deposit: u128, tgas: u64) -> near_workspaces::operations::CallTransaction {
    who.call(&to.parse().unwrap(), method).args_json(args).deposit(NearToken::from_yoctonear(deposit)).gas(Gas::from_tgas(tgas))
}

// TC-CT-098-01: the life of an epoch with three members and fees, from `join` to `prune_epoch`, `request_exit` and `withdraw_stake`
#[tokio::test]
async fn gas_of_the_life_of_an_epoch() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["fees"] = json!({"clearing_fee_bps": 10, "solver_fee_bps": 5});
    params["admission"] = json!("open");
    let mut f = Funded::deploy(Some(params)).await?;
    let g = "epoch";
    for name in ["scraper", "gpu", "translator"] {
        let account = f.account(name).await?;
        let before = storage(&f).await?;
        let r = f.join(name, Funded::join_deposit()).await?;
        line(&f, g, "join", before, &r, "JOIN_STORAGE_DEPOSIT 0.35 NEAR covers the worst case of one member").await?;
        let before = storage(&f).await?;
        let r = f.stake(&account, 100 * CREDIT).await?;
        line(&f, g, "ft_on_transfer (stake, through ft_transfer_call)", before, &r, "includes the transfer on the token").await?;
    }
    let scraper = f.account("scraper").await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&scraper, "post_intent", json!({"intent_hash": near_sdk::bs58::encode([1u8; 32]).into_string()})).await?;
    line(&f, g, "post_intent", before, &r, "").await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&scraper, "remove_intent", json!({"intent_hash": near_sdk::bs58::encode([1u8; 32]).into_string()})).await?;
    line(&f, g, "remove_intent", before, &r, "frees the entry").await?;
    for (i, (payer, payee, amount)) in [("scraper", "gpu", 60), ("gpu", "translator", 25), ("translator", "scraper", 25)].into_iter().enumerate() {
        let before = storage(&f).await?;
        let r = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        line(&f, g, if i == 0 { "register_cumulative (first voucher of a pair and epoch)" } else { "register_cumulative (a new pair)" }, before, &r, "pair, totals and edge list of the epoch").await?;
    }
    let before = storage(&f).await?;
    let r = f.circle.register("scraper", "gpu", 1, 61 * CREDIT).await?;
    line(&f, g, "register_cumulative (a later voucher of a known pair)", before, &r, "no new entry").await?;
    let before = storage(&f).await?;
    let r = common::flow::request_exit(&f, "translator").await?;
    line(&f, g, "request_exit", before, &r, "").await?;
    let c = &f.circle;
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    let r = measure!(&f, g, "close_epoch", "", f.circle.call_keeper("close_epoch", json!({"epoch": 1})));
    assert!(r.is_success());
    let edges = json!([edge("scraper", "gpu", 25 * CREDIT), edge("gpu", "translator", 25 * CREDIT), edge("translator", "scraper", 25 * CREDIT)]);
    let r = measure!(&f, g, "submit_clearing (3 edges)", "stores the proposal", f.circle.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})));
    assert!(r.is_success(), "{:?}", failure_text(&r));
    f.circle.advance_to(f.circle.epoch_ts(1, "proposal_deadline").await?).await?;
    let r = measure!(&f, g, "commit_epoch (3 edges, fees on)", "positions of the epoch; fees move stake to the reserve and the solver", f.circle.call_keeper("commit_epoch", json!({"epoch": 1})));
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let owed = f.circle.view("net_position", json!({"epoch": 1, "account_id": account_of("scraper")})).await?["amount"].as_str().unwrap().parse::<u128>()?;
    let before = storage(&f).await?;
    let r = f.transfer_call(&f.usdt, &scraper, owed, &json!({"action": "settle", "epoch": 1}).to_string()).await?;
    line(&f, g, "ft_on_transfer (settle, residual payment)", before, &r, "includes the transfer on the token").await?;
    let r = measure!(&f, g, "finalize_epoch", "", common::flow::finalize(&f.circle));
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let gpu = f.accounts["gpu"].clone();
    let before = storage(&f).await?;
    let r = tx(&gpu, f.circle.contract.id().as_str(), "claim", json!({"epoch": 1, "destination": "near"}), 1, 300).transact().await?;
    line(&f, g, "claim (to a NEAR account, with on_claim_resolved)", before, &r, "includes the transfer on the token and the callback").await?;
    let before = storage(&f).await?;
    let r = tx(&solver, f.circle.contract.id().as_str(), "claim_solver_fee", json!({}), 1, 300).transact().await?;
    line(&f, g, "claim_solver_fee (with on_solver_fee_resolved)", before, &r, "includes the transfer on the token and the callback").await?;
    common::flow::pass_prune_delay(&f.circle).await?;
    let r = measure!(&f, g, "prune_epoch (max_items 1000)", "frees the entries of the epoch", f.circle.call_as(&scraper, "prune_epoch", json!({"epoch": 1, "max_items": 1000})));
    let _ = r;
    let before = storage(&f).await?;
    let r = common::flow::withdraw_stake(&f, "translator").await?;
    line(&f, g, "withdraw_stake (with on_withdraw_resolved)", before, &r, "includes the transfer on the token and the callback").await?;
    Ok(())
}

// TC-CT-098-02: bundles, deals with a bond, the freeze, the registry, the parameters, admission and the upgrade
#[tokio::test]
async fn gas_of_bundles_deals_freeze_and_admin() -> anyhow::Result<()> {
    const HOUR: u64 = 3600 * 1_000_000_000;
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    let g = "rules";
    for name in ["alice", "bob", "carol"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 800 * CREDIT).await?.is_success());
    }
    let (alice, bob, dao) = (f.accounts["alice"].clone(), f.accounts["bob"].clone(), f.account("dao").await?);
    let arb = f.account("arb").await?;
    let arbiter = arb.id().to_string();
    let now = f.circle.now_ns().await?;
    let agreement = "ab".repeat(32);

    // a bundle without a deal, and a voucher of one of its legs
    let before = storage(&f).await?;
    let plain = f.circle.commit_bundle(1, &[("alice", "bob", 10 * CREDIT), ("bob", "alice", 8 * CREDIT)], &["alice", "bob"]).await?;
    line(&f, g, "commit_bundle (2 participants, 2 legs)", before, &plain, "reserves headroom; the cases with 64 legs are in ct086 and ct043").await?;
    let plain_id = plain.json::<u64>()?;
    let before = storage(&f).await?;
    let r = f.circle.register_in_bundle(plain_id, "alice", "bob", 1, 10 * CREDIT).await?;
    line(&f, g, "register_in_bundle", before, &r, "same path as register_cumulative").await?;

    // a deal with a bond that both parties confirm
    let before = storage(&f).await?;
    let deal = f.circle.commit_bundle_with_deal(1, &[("alice", "bob", 120 * CREDIT), ("bob", "alice", 100 * CREDIT)], (&arbiter, Some(&agreement), now + 48 * HOUR), &["alice", "bob"]).await?;
    line(&f, g, "commit_bundle (2 participants, 2 legs, with arbiter: opens a deal and locks 2 bonds)", before, &deal, "a Deal record, 2 bond_locked events").await?;
    let one = deal.json::<u64>()?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&alice, "confirm_deal", json!({"bundle_id": one})).await?;
    line(&f, g, "confirm_deal (first party)", before, &r, "releases the bond of the other party").await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&bob, "confirm_deal", json!({"bundle_id": one})).await?;
    line(&f, g, "confirm_deal (second party, closes the deal)", before, &r, "deletes the Deal record").await?;

    // a deal that is disputed and ruled, one per verdict, and one that nobody settles
    let mut ids = vec![];
    for (i, amount) in [(0, 110u128), (1, 111), (2, 112), (3, 113)] {
        let r = f.circle.commit_bundle_with_deal(1, &[("alice", "carol", amount * CREDIT), ("carol", "alice", 50 * CREDIT)], (&arbiter, None, now + if i == 3 { 2 * HOUR } else { (24 + i) * HOUR }), &["alice", "carol"]).await?;
        assert!(r.is_success(), "{:?}", failure_text(&r));
        ids.push(r.json::<u64>()?);
    }
    let evidence = "cd".repeat(32);
    for (k, verdict) in ["accused_guilty", "complaint_false", "no_fault"].into_iter().enumerate() {
        let carol = f.accounts["carol"].clone();
        let before = storage(&f).await?;
        let r = f.circle.call_as(&carol, "dispute_deal", json!({"bundle_id": ids[k], "evidence_hash": evidence})).await?;
        if k == 0 {
            line(&f, g, "dispute_deal", before, &r, "").await?;
        }
        let before = storage(&f).await?;
        let r = f.circle.call_as(&arb, "rule_deal", json!({"bundle_id": ids[k], "verdict": verdict})).await?;
        line(&f, g, &format!("rule_deal ({verdict})"), before, &r, "moves stake between members and may lower limits (limits_updated)").await?;
    }
    f.circle.advance_to(now + 2 * HOUR + 120_000_000_000).await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "settle_deal", json!({"bundle_id": ids[3]})).await?;
    line(&f, g, "settle_deal (window over, nobody complained)", before, &r, "releases the bonds").await?;

    // the freeze
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "propose_freeze", json!({"account_id": account_of("bob"), "reason_hash": evidence})).await?;
    line(&f, g, "propose_freeze", before, &r, "a FreezeRecord of about 200 bytes").await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "cancel_freeze", json!({"account_id": account_of("bob")})).await?;
    line(&f, g, "cancel_freeze", before, &r, "").await?;
    assert!(f.circle.call_as(&dao, "propose_freeze", json!({"account_id": account_of("bob"), "reason_hash": null})).await?.is_success());
    // the timelock of 24 hours is moved into the past as ct095 does (a jump of 24 hours is 86,400 blocks in the sandbox)
    let mut record = vec![0u8];
    record.extend_from_slice(&1u64.to_le_bytes());
    record.push(0);
    record.extend_from_slice(&0u64.to_le_bytes());
    let key = [b"x207_freeze:".as_slice(), account_of("bob").as_bytes()].concat();
    f.circle.worker.patch_state(f.circle.contract.id(), &key, &record).await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&alice, "apply_freeze", json!({"account_id": account_of("bob")})).await?;
    line(&f, g, "apply_freeze (after the 24 hour timelock)", before, &r, "").await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "unfreeze_member", json!({"account_id": account_of("bob")})).await?;
    line(&f, g, "unfreeze_member", before, &r, "deletes the FreezeRecord").await?;

    // the registry and the parameters
    let entry = json!({"asset_type": "rwa:bond", "class": "A", "delivery_methods": ["escrow_ft"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "rwa:bond", "entry": entry})).await?;
    line(&f, g, "set_asset_class", before, &r, "one entry").await?;
    let list: Vec<String> = (0..16).map(|i| format!("token{i}.test.near")).collect();
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "set_asset_contracts", json!({"asset_type": "rwa:bond", "contracts": list})).await?;
    line(&f, g, "set_asset_contracts (16 contracts, the most)", before, &r, "").await?;
    let params = f.circle.view("get_params", json!({})).await?;
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "set_params", json!({"params": params})).await?;
    line(&f, g, "set_params (pending for the next epoch)", before, &r, "also writes the bond rules").await?;

    // the upgrade: propose, wait out the timelock (patched in the sandbox as in ct083), apply with the same wasm
    let wasm = std::fs::read("target/near/x207_circle.wasm")?;
    let hash = near_sdk::bs58::encode(sha2::Sha256::digest(&wasm)).into_string();
    let before = storage(&f).await?;
    let r = f.circle.call_as(&dao, "propose_upgrade", json!({"code_hash": hash})).await?;
    line(&f, g, "propose_upgrade", before, &r, "").await?;
    let mut stored = sha2::Sha256::digest(&wasm).to_vec();
    stored.extend_from_slice(&1u64.to_le_bytes());
    f.circle.worker.patch_state(f.circle.contract.id(), b"x207_upgrade", &stored).await?;
    f.circle.worker.fast_forward(2).await?;
    let before = storage(&f).await?;
    let r = dao.call(f.circle.contract.id(), "apply_upgrade").args(wasm).gas(Gas::from_tgas(300)).transact().await?;
    line(&f, g, "apply_upgrade (deploys the code and runs migrate)", before, &r, "migrate is part of this transaction").await?;
    Ok(())
}

use sha2::Digest;

// TC-CT-098-03: the escrow of a class A asset, three forms: NFT (NEP-171), NEP-141 token, NEP-245 multi-token. Per form: the offer, the deposit, the purchase, and the way back.
#[tokio::test]
async fn gas_of_the_escrow_of_an_nft() -> anyhow::Result<()> {
    let mut f = Funded::deploy(None).await?;
    for name in ["alice", "bob"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 150 * CREDIT).await?.is_success());
    }
    let dao = f.account("dao").await?;
    let entry = json!({"asset_type": "nft:ticket", "class": "A", "delivery_methods": ["escrow_nft"], "accepted_proofs": [], "terms_hash": "5a".repeat(32)});
    assert!(f.circle.call_as(&dao, "set_asset_class", json!({"asset_type": "nft:ticket", "entry": entry})).await?.is_success());
    let nft = f.circle.worker.dev_deploy(&std::fs::read(common::probe_wasm("mock_nft"))?).await?;
    nft.call("new").transact().await?.into_result()?;
    let (alice, bob) = (f.accounts["alice"].clone(), f.accounts["bob"].clone());
    let g = "escrow nft";
    for (round, token) in ["t1", "t2"].into_iter().enumerate() {
        nft.call("nft_mint").args_json(json!({"token_id": token, "owner_id": alice.id()})).transact().await?.into_result()?;
        let expires = f.circle.now_ns().await? + if round == 0 { 3_600_000_000_000 } else { 90_000_000_000 };
        let input = json!({"input": {"asset_type": "nft:ticket", "delivery_method": "escrow_nft",
            "limits": {"scope": json!({"nft_contract": nft.id(), "token_id": token}).to_string(), "quota": null, "expires_at": expires.to_string()},
            "unit": "once", "unit_price": (10 * CREDIT).to_string(), "operator": null, "proof_hash": null, "proof_kind": null}});
        let before = storage(&f).await?;
        let r = f.circle.call_as(&alice, "offer_grant", input).await?;
        if round == 0 {
            line(&f, g, "offer_grant (class A, NFT scope)", before, &r, "").await?;
        }
        let id = r.json::<u64>()?;
        let before = storage(&f).await?;
        let r = alice.call(nft.id(), "nft_transfer_call").args_json(json!({"receiver_id": f.circle.contract.id(), "token_id": token, "msg": json!({"grant_id": id}).to_string()}))
            .deposit(NearToken::from_yoctonear(1)).gas(Gas::from_tgas(150)).transact().await?;
        if round == 0 {
            line(&f, g, "nft_on_transfer (deposit through nft_transfer_call)", before, &r, "an Escrow record; includes the transfer on the NFT contract").await?;
        }
        if round == 0 {
            let (voucher, signature) = f.circle.next_voucher("bob", "alice", 1, 10 * CREDIT);
            let before = storage(&f).await?;
            let r = f.circle.call_as(&bob, "complete_escrow", json!({"grant_id": id, "voucher": voucher, "signature": signature})).await?;
            line(&f, g, "complete_escrow (with on_escrow_resolved)", before, &r, "registers the voucher and delivers the NFT").await?;
        } else {
            f.circle.advance_to(expires + 2_000_000_000).await?;
            let before = storage(&f).await?;
            let r = f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id})).await?;
            line(&f, g, "cancel_escrow (after expiry, with on_escrow_resolved)", before, &r, "gives the NFT back").await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn gas_of_the_escrow_of_a_token() -> anyhow::Result<()> {
    let mut w = ft_escrow::world().await?;
    let (alice, g) = (w.alice.clone(), "escrow nep-141");
    let id = {
        let expires = w.f.circle.now_ns().await? + 3_600_000_000_000;
        let before = storage(&w.f).await?;
        let r = w.f.circle.call_as(&alice, "offer_grant", ft_escrow::offer(expires, w.rwa.id().as_str(), ft_escrow::AMOUNT)).await?;
        line(&w.f, g, "offer_grant (class A, NEP-141 scope)", before, &r, "").await?;
        r.json::<u64>()?
    };
    let before = storage(&w.f).await?;
    let r = w.deposit(&alice, ft_escrow::AMOUNT, id).await?;
    line(&w.f, g, "ft_on_transfer (escrow action, through ft_transfer_call)", before, &r, "an FtEscrow record; includes the transfer on the token").await?;
    let epoch = w.epoch().await?;
    let before = storage(&w.f).await?;
    let r = w.buy("bob", epoch, ft_escrow::PRICE, id).await?;
    line(&w.f, g, "complete_escrow (with on_escrow_resolved)", before, &r, "registers the voucher and sends the tokens").await?;
    let id2 = w.offer(90_000_000_000).await?;
    assert!(w.deposit(&alice, ft_escrow::AMOUNT, id2).await?.is_success());
    let expires = w.f.circle.view("get_grant", json!({"grant_id": id2})).await?["limits"]["expires_at"].as_str().unwrap().parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let before = storage(&w.f).await?;
    let r = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id2})).await?;
    line(&w.f, g, "cancel_escrow (after expiry, with on_escrow_resolved)", before, &r, "gives the tokens back").await?;
    Ok(())
}

#[tokio::test]
async fn gas_of_the_escrow_of_a_multi_token() -> anyhow::Result<()> {
    let mut w = mt_escrow::world().await?;
    let (alice, g) = (w.alice.clone(), "escrow nep-245");
    let id = {
        let expires = w.f.circle.now_ns().await? + 3_600_000_000_000;
        let before = storage(&w.f).await?;
        let r = w.f.circle.call_as(&alice, "offer_grant", mt_escrow::offer(expires, w.mt.id().as_str(), mt_escrow::TOKEN, mt_escrow::AMOUNT)).await?;
        line(&w.f, g, "offer_grant (class A, NEP-245 scope)", before, &r, "").await?;
        r.json::<u64>()?
    };
    let before = storage(&w.f).await?;
    let r = w.deposit(&alice, mt_escrow::AMOUNT, id).await?;
    line(&w.f, g, "mt_on_transfer (escrow action, through mt_transfer_call)", before, &r, "an MtEscrow record; includes the transfer on the multi-token").await?;
    let epoch = w.epoch().await?;
    let before = storage(&w.f).await?;
    let r = w.buy("bob", epoch, mt_escrow::PRICE, id).await?;
    line(&w.f, g, "complete_escrow (with on_escrow_resolved)", before, &r, "registers the voucher and sends the tokens").await?;
    let id2 = w.offer(90_000_000_000).await?;
    assert!(w.deposit(&alice, mt_escrow::AMOUNT, id2).await?.is_success());
    let expires = w.f.circle.view("get_grant", json!({"grant_id": id2})).await?["limits"]["expires_at"].as_str().unwrap().parse::<u64>()?;
    w.f.circle.advance_to(expires + 2_000_000_000).await?;
    let before = storage(&w.f).await?;
    let r = w.f.circle.call_as(&alice, "cancel_escrow", json!({"grant_id": id2})).await?;
    line(&w.f, g, "cancel_escrow (after expiry, with on_escrow_resolved)", before, &r, "gives the tokens back").await?;
    Ok(())
}

// TC-CT-098-04: the clearing of 300 edges (the limit of one `submit_clearing`), with fees: 30 members, each owes the next `X207_GAS_OFFSETS` members (10) one credit
#[tokio::test]
async fn gas_of_a_clearing_of_300_edges() -> anyhow::Result<()> {
    let mut params = Funded::params_short();
    params["fees"] = json!({"clearing_fee_bps": 10, "solver_fee_bps": 5});
    params["proposal_window"] = json!(600);
    let mut f = Funded::deploy(Some(params)).await?;
    let n = 30usize;
    let name = |i: usize| format!("m{i:02}");
    for i in 0..n {
        let account = f.account(&name(i)).await?;
        assert!(f.join(&name(i), Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    let mut edges = vec![];
    let offsets: usize = std::env::var("X207_GAS_OFFSETS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    for k in 1..=offsets {
        for i in 0..n {
            let (payer, payee) = (name(i), name((i + k) % n));
            let r = f.circle.register(&payer, &payee, 1, CREDIT).await?;
            assert!(r.is_success(), "register {payer}->{payee}: {:?}", failure_text(&r));
            edges.push(edge(&payer, &payee, CREDIT));
        }
    }
    let g = "clearing 300";
    let solver = f.circle.worker.dev_create_account().await?;
    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    let r = measure!(&f, g, "close_epoch (300 edges in the epoch)", "", f.circle.call_keeper("close_epoch", json!({"epoch": 1})));
    assert!(r.is_success(), "{:?}", failure_text(&r));
    let r = measure!(&f, g, &format!("submit_clearing ({} edges, 30 members)", n * offsets), "stores the proposal", f.circle.call_as(&solver, "submit_clearing", json!({"epoch": 1, "edges": edges})));
    assert!(r.is_success(), "{:?}", failure_text(&r));
    f.circle.advance_to(f.circle.epoch_ts(1, "proposal_deadline").await?).await?;
    let r = measure!(&f, g, &format!("commit_epoch ({} edges, 30 members, fees on)", n * offsets), "30 positions; the edges are dropped", f.circle.call_keeper("commit_epoch", json!({"epoch": 1})));
    let _ = r; // a refusal is a result of the measurement and is printed as REFUSED
    Ok(())
}
