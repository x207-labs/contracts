//! CT-054: several net debtors default in the same epoch and nobody is cascaded (RIS-18): while `limit <= stake`
//! the stakes cover the whole residual, so every creditor is paid in full. Needs
//! `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::finalize;
use common::funded::Funded;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::AccountId;
use serde_json::json;

async fn usdt(f: &Funded, name: &str) -> anyhow::Result<u128> {
    let id: AccountId = account_of(name).parse()?;
    f.balance(&f.usdt, &id).await
}

#[tokio::test]
async fn three_debtors_default_and_the_creditors_are_paid_in_full() -> anyhow::Result<()> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["d1", "d2", "d3", "c1", "c2"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    // no cycle: nothing is cleared, three debtors owe 30, 20 and 40 to two creditors
    for (payer, payee, amount) in [("d1", "c1", 30), ("d2", "c1", 20), ("d3", "c2", 40)] {
        let r = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(r.is_success(), "{payer}->{payee}: {:?}", failure_text(&r));
    }
    let c = &f.circle;
    c.advance_to(c.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(c.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    assert!(c.call_keeper("commit_epoch", json!({"epoch": 1})).await?.is_success());

    let before = (usdt(&f, "c1").await?, usdt(&f, "c2").await?);
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));
    let log = events(&fin);
    let defaulted: Vec<_> = log.iter().filter(|(n, _)| n == "member_defaulted").collect();
    assert_eq!(defaulted.len(), 3, "three debtors default");
    for (_, d) in &defaulted {
        assert_eq!(d["shortfall"], "0", "the stake covers the residual: {d}");
        assert_eq!(d["from_reserve"], "0");
    }
    let finalized = log.iter().find(|(n, _)| n == "epoch_finalized").expect("epoch_finalized").1.clone();
    assert_eq!((finalized["defaulted_count"].as_u64(), finalized["total_defaulted"].as_str(), finalized["shortfall"].as_str()), (Some(3), Some("90000000"), Some("0")));
    f.assert_d6().await?;

    for (name, owed) in [("c1", 50), ("c2", 40)] {
        let claim = f.accounts[name]
            .call(f.circle.contract.id(), "claim")
            .args_json(json!({"epoch": 1, "destination": "near"}))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(300))
            .transact()
            .await?;
        assert!(claim.is_success(), "{name}: {:?}", failure_text(&claim));
        let _ = owed;
    }
    assert_eq!(usdt(&f, "c1").await?, before.0 + 50 * CREDIT, "c1 is paid in full, no haircut");
    assert_eq!(usdt(&f, "c2").await?, before.1 + 40 * CREDIT, "c2 is paid in full, no haircut");
    for name in ["d1", "d2", "d3"] {
        let m = f.member(name).await?;
        assert_eq!(m["status"], "suspended", "{name}");
    }
    f.assert_d6().await?;
    Ok(())
}
