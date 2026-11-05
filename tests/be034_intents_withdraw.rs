//! BE-034, contract side (spec 18): `withdraw_intents_balance` brings the USDT that `mt_on_transfer` booked from the multi-token balance at Intents to the
//! NEP-141 account of the circle. `probes/mock_intents` holds balances and plays `mt_transfer_call` and `ft_withdraw`; the USDT behind its balances is
//! its own NEP-141 balance, which a test funds first, as a deposit to Intents would. Needs `cargo near build non-reproducible-wasm` first.

mod common;

use common::circle::{account_of, events, failure_text, CREDIT};
use common::flow::{finalize, s1_committed_on};
use common::funded::Funded;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::Account;
use serde_json::json;

async fn window() -> anyhow::Result<Funded> {
    s1_committed_on(Funded::deploy_with_intents(Some(Funded::params_short())).await?).await
}

/// `who` deposits `amount` USDT to Intents: the NEP-141 tokens move to the Intents account (the multi-token balance is made by the next call).
async fn fund_intents(f: &Funded, who: &Account, amount: u128) -> anyhow::Result<()> {
    let intents = f.intents.as_ref().expect("mock intents");
    let r = who
        .call(f.usdt.id(), "ft_transfer")
        .args_json(json!({"receiver_id": intents.id(), "amount": amount.to_string(), "memo": null}))
        .deposit(NearToken::from_yoctonear(1))
        .transact()
        .await?;
    assert!(r.is_success(), "{:?}", failure_text(&r));
    Ok(())
}

async fn pay_through_intents(
    f: &Funded,
    debtor: &str,
    amount: u128,
) -> anyhow::Result<ExecutionFinalResult> {
    let intents = f.intents.as_ref().expect("mock intents");
    let caller = f.circle.worker.dev_create_account().await?;
    Ok(caller
        .call(intents.id(), "mt_transfer_call")
        .args_json(json!({
            "receiver_id": f.circle.contract.id(), "previous_owner_id": account_of(debtor), "token_id": format!("nep141:{}", f.usdt.id()),
            "amount": amount.to_string(), "msg": json!({"action": "settle", "epoch": 1}).to_string()
        }))
        .gas(Gas::from_tgas(200))
        .transact()
        .await?)
}

async fn withdraw(f: &Funded, gas_tgas: u64) -> anyhow::Result<ExecutionFinalResult> {
    let anyone = f.circle.worker.dev_create_account().await?;
    Ok(anyone
        .call(f.circle.contract.id(), "withdraw_intents_balance")
        .gas(Gas::from_tgas(gas_tgas))
        .transact()
        .await?)
}

async fn claim(f: &Funded, who: &Account) -> anyhow::Result<ExecutionFinalResult> {
    Ok(who
        .call(f.circle.contract.id(), "claim")
        .args_json(json!({"epoch": 1, "destination": "near"}))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?)
}

async fn at_intents(f: &Funded) -> anyhow::Result<String> {
    let intents = f.intents.as_ref().expect("mock intents");
    Ok(intents
        .view("balance_of")
        .args_json(json!({"account_id": f.circle.contract.id()}))
        .await?
        .json::<String>()?)
}

async fn booked(f: &Funded) -> anyhow::Result<String> {
    Ok(f.circle
        .view("intents_balance", json!({}))
        .await?
        .as_str()
        .unwrap()
        .to_string())
}

/// How much more the pool says the circle holds than its NEP-141 account does.
async fn unbacked(f: &Funded) -> anyhow::Result<i128> {
    let pool = f.circle.view("get_pool", json!({})).await?;
    let accounted: i128 = pool["accounted"].as_str().unwrap().parse()?;
    Ok(accounted - f.balance(&f.usdt, f.circle.contract.id()).await? as i128)
}

// TC-BE-034-10..16: pool booked at mt_on_transfer, NEP-141 short, claim fails and can be repeated, withdrawal fixes it
#[tokio::test]
async fn the_booked_pool_is_backed_after_the_withdrawal_and_a_failed_claim_can_be_repeated(
) -> anyhow::Result<()> {
    let f = window().await?;
    let scraper = f.accounts["scraper"].clone();
    fund_intents(&f, &scraper, 35 * CREDIT).await?;
    let paid = pay_through_intents(&f, "scraper", 35 * CREDIT).await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    // the pool has the 35, the NEP-141 account of the circle has not, and the contract knows where they are
    assert_eq!(at_intents(&f).await?, "35000000");
    assert_eq!(booked(&f).await?, "35000000");
    assert_eq!(unbacked(&f).await?, 35 * CREDIT as i128);
    let fin = finalize(&f.circle).await?;
    assert!(fin.is_success(), "{:?}", failure_text(&fin));

    // the NEP-141 balance of the circle is short (the stakes were paid out): the claim fails the way a failed claim does
    let balance = f.balance(&f.usdt, f.circle.contract.id()).await?;
    let burn = f.usdt.call("burn").args_json(json!({"account_id": f.circle.contract.id(), "amount": (balance - 10 * CREDIT).to_string()})).transact().await?;
    assert!(burn.is_success(), "{:?}", failure_text(&burn));
    let gpu = f.accounts["gpu"].clone();
    let gpu_before = f.balance(&f.usdt, gpu.id()).await?;
    let failed = claim(&f, &gpu).await?;
    let log = events(&failed);
    assert!(
        log.iter().any(|(n, d)| n == "residual_claim_failed"
            && d["amount"] == "35000000"
            && d["status"] == "claimable"),
        "{log:?}"
    );
    assert_eq!(
        f.circle
            .view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of("gpu")})
            )
            .await?["status"],
        "claimable"
    );
    assert_eq!(f.balance(&f.usdt, gpu.id()).await?, gpu_before);

    // the withdrawal: permissionless, the whole booked amount, one event, the books agree
    let before = f.balance(&f.usdt, f.circle.contract.id()).await?;
    let done = withdraw(&f, 300).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(done.clone().json::<String>()?, "35000000");
    assert!(events(&done)
        .iter()
        .any(|(n, d)| n == "intents_withdrawn" && d["amount"] == "35000000"));
    assert_eq!(
        f.balance(&f.usdt, f.circle.contract.id()).await?,
        before + 35 * CREDIT
    );
    assert_eq!(
        (at_intents(&f).await?.as_str(), booked(&f).await?.as_str()),
        ("0", "0")
    );
    let again = withdraw(&f, 300).await?;
    assert!(failure_text(&again)
        .expect("refused")
        .contains("ERR_NOTHING_TO_WITHDRAW"));

    // the claim that failed is repeated and pays
    let paid_claim = claim(&f, &gpu).await?;
    assert!(paid_claim.is_success(), "{:?}", failure_text(&paid_claim));
    assert_eq!(
        f.balance(&f.usdt, gpu.id()).await?,
        gpu_before + 35 * CREDIT
    );
    assert_eq!(
        f.circle
            .view(
                "net_position",
                json!({"epoch": 1, "account_id": account_of("gpu")})
            )
            .await?["status"],
        "claimed"
    );
    Ok(())
}

// TC-BE-034-17..19: a withdrawal that Intents cannot finish books nothing and is repeated; too little gas; nothing booked
#[tokio::test]
async fn a_failed_withdrawal_books_nothing_and_can_be_called_again() -> anyhow::Result<()> {
    let f = window().await?;
    let nothing = withdraw(&f, 300).await?;
    assert!(failure_text(&nothing)
        .expect("refused")
        .contains("ERR_NOTHING_TO_WITHDRAW"));

    // Intents holds no NEP-141 tokens behind the balance (nobody deposited), so its transfer fails and it answers 0
    let paid = pay_through_intents(&f, "scraper", 35 * CREDIT).await?;
    assert!(paid.is_success(), "{:?}", failure_text(&paid));
    assert_eq!(booked(&f).await?, "35000000");
    let low = withdraw(&f, 60).await?;
    assert!(failure_text(&low)
        .expect("refused")
        .contains("ERR_GAS_TOO_LOW"));
    let failed = withdraw(&f, 300).await?;
    assert!(failed.is_success(), "{:?}", failure_text(&failed));
    assert_eq!(
        failed.clone().json::<String>()?,
        "0",
        "nothing left Intents"
    );
    assert!(events(&failed)
        .iter()
        .all(|(n, _)| n != "intents_withdrawn"));
    assert_eq!(
        (booked(&f).await?.as_str(), at_intents(&f).await?.as_str()),
        ("35000000", "35000000"),
        "still booked, still at Intents"
    );

    // the deposit arrives at Intents; the same call now works
    let scraper = f.accounts["scraper"].clone();
    fund_intents(&f, &scraper, 35 * CREDIT).await?;
    let done = withdraw(&f, 300).await?;
    assert!(done.is_success(), "{:?}", failure_text(&done));
    assert_eq!(done.clone().json::<String>()?, "35000000");
    assert_eq!(booked(&f).await?, "0");
    Ok(())
}

// TC-BE-034-20: one withdrawal at a time
#[tokio::test]
async fn a_second_withdrawal_while_one_is_in_flight_is_refused() -> anyhow::Result<()> {
    let f = window().await?;
    let scraper = f.accounts["scraper"].clone();
    fund_intents(&f, &scraper, 35 * CREDIT).await?;
    assert!(pay_through_intents(&f, "scraper", 35 * CREDIT)
        .await?
        .is_success());
    let (a, b) = tokio::join!(withdraw(&f, 300), withdraw(&f, 300));
    let (a, b) = (a?, b?);
    let (won, lost) = if a.is_success() { (a, b) } else { (b, a) };
    assert!(won.is_success(), "{:?}", failure_text(&won));
    let text = failure_text(&lost).expect("the second call is refused");
    assert!(
        text.contains("ERR_WITHDRAW_IN_FLIGHT") || text.contains("ERR_NOTHING_TO_WITHDRAW"),
        "{}",
        text.chars().take(200).collect::<String>()
    );
    assert_eq!(
        unbacked(&f).await?,
        0,
        "after the one withdrawal the pool is backed"
    );
    assert_eq!(booked(&f).await?, "0");
    Ok(())
}
