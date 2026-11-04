//! SV-006 in near-sandbox: the real solver CLI (`../solver/dist/cli.js`, built with `npm run build`)
//! runs against a real circle. It reads the epoch snapshot from a stub read API (built from the
//! contract's `list_obligations`), simulates, signs and sends `submit_clearing` over the sandbox RPC.
//! Skipped when the solver repo is not next to this one. Needs `cargo near build non-reproducible-wasm`.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use common::circle::{account_of, failure_text, Circle, CREDIT};
use common::flow::edge;
use common::funded::Funded;
use serde_json::{json, Value};

const CLI: &str = "../solver/dist/cli.js";

/// S2 (analyst, coder, summarizer; four pairs), closed. Returns the funded circle.
async fn s2_closed() -> anyhow::Result<Funded> {
    let mut f = Funded::deploy(Some(Funded::params_short())).await?;
    for name in ["summarizer", "analyst", "coder"] {
        let account = f.account(name).await?;
        assert!(f.join(name, Funded::join_deposit()).await?.is_success());
        assert!(f.stake(&account, 100 * CREDIT).await?.is_success());
    }
    for (payer, payee, amount) in [("summarizer", "analyst", 15), ("analyst", "summarizer", 15), ("analyst", "coder", 10), ("coder", "summarizer", 10)] {
        let r = f.circle.register(payer, payee, 1, amount * CREDIT).await?;
        assert!(r.is_success(), "register {payer}->{payee}: {:?}", failure_text(&r));
    }
    f.circle.advance_to(f.circle.epoch_ts(1, "cutoff_at").await?).await?;
    assert!(f.circle.call_keeper("close_epoch", json!({"epoch": 1})).await?.is_success());
    Ok(f)
}

/// The read API snapshot of spec 3.3 for the closed epoch, from the contract's own pair entries.
async fn snapshot(c: &Circle) -> anyhow::Result<Value> {
    let pairs = c.view("list_obligations", json!({"epoch": 1, "from_index": 0, "limit": 100})).await?;
    let edges: Vec<Value> = pairs
        .as_array()
        .unwrap()
        .iter()
        .map(|p| json!({"debtor": p["debtor"], "creditor": p["creditor"], "amount": p["cumulative"], "seq": p["last_seq"], "cleared": null, "remaining": null}))
        .collect();
    let total: u128 = edges.iter().map(|e| e["amount"].as_str().unwrap().parse::<u128>().unwrap()).sum();
    Ok(json!({"circle": "agents-main", "epoch": 1, "phase": "clearing", "stage": "cutoff", "unit": "credit", "decimals": 6,
              "edgeCount": edges.len(), "totalObligation": total.to_string(), "edges": edges}))
}

/// A one-thread HTTP server that answers every request with `body`. Returns its base URL.
fn serve(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    url
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn run_solver(f: &Funded, read_api: &str, solver: &near_workspaces::Account) -> anyhow::Result<Run> {
    let env = [
        ("X207_READ_API_URL", read_api.to_string()),
        ("X207_CIRCLE_ID", "agents-main".to_string()),
        ("NEAR_RPC_URL", format!("http://{}", f.circle.worker.rpc_addr().trim_start_matches("http://"))),
        ("X207_CONTRACT_ID", f.circle.contract.id().to_string()),
        ("SOLVER_ACCOUNT_ID", solver.id().to_string()),
        ("SOLVER_PRIVATE_KEY", solver.secret_key().to_string()),
        ("PATH", std::env::var("PATH")?),
    ];
    let output = tokio::task::spawn_blocking(move || {
        Command::new("node").arg(CLI).args(["--epoch", "1"]).env_clear().envs(env).output()
    })
    .await??;
    Ok(Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into(),
        stderr: String::from_utf8_lossy(&output.stderr).into(),
    })
}

fn solver_built() -> bool {
    let ok = Path::new(CLI).exists();
    if !ok {
        eprintln!("skipped: {CLI} not found (build the solver with `npm run build`)");
    }
    ok
}

async fn stored(c: &Circle) -> anyhow::Result<Value> {
    c.view("get_clearing_proposal", json!({"epoch": 1})).await
}

// TC-SV-006-16, TC-SV-006-17: the proposal is accepted and replaces a smaller one; after commit the nets stay
#[tokio::test]
async fn solver_submits_and_beats_a_smaller_proposal() -> anyhow::Result<()> {
    if !solver_built() {
        return Ok(());
    }
    let f = s2_closed().await?;
    let c = &f.circle;
    let read_api = serve(snapshot(c).await?.to_string());
    let (solver_a, solver) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);

    let small = json!([edge("summarizer", "analyst", 10 * CREDIT), edge("analyst", "coder", 10 * CREDIT), edge("coder", "summarizer", 10 * CREDIT)]);
    assert!(c.call_as(&solver_a, "submit_clearing", json!({"epoch": 1, "edges": small})).await?.is_success());
    assert_eq!(stored(c).await?["cleared_total"], "30000000");

    let run = run_solver(&f, &read_api, &solver).await?;
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("submit_clearing accepted"), "{}", run.stderr);
    assert!(run.stderr.contains(&format!("{} 30000000", solver_a.id())), "the replaced proposal is logged: {}", run.stderr);
    let proposal = stored(c).await?;
    assert_eq!(proposal["solver"].as_str(), Some(solver.id().as_str()));
    assert_eq!((proposal["cleared_total"].as_str(), proposal["edge_count"].as_u64(), proposal["committed"].as_bool()), (Some("40000000"), Some(4), Some(false)));
    assert_eq!(serde_json::from_str::<Value>(run.stdout.trim())?["edges"].as_array().unwrap().len(), 4);

    // after commit the net positions are those of the obligations: analyst owes 10, summarizer is owed 10
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;
    assert!(c.call_keeper("commit_epoch", json!({"epoch": 1})).await?.is_success());
    let bill = c.view("net_position", json!({"epoch": 1, "account_id": account_of("analyst")})).await?;
    assert_eq!((bill["role"].as_str(), bill["amount"].as_str()), (Some("debtor"), Some("10000000")));
    let owed = c.view("net_position", json!({"epoch": 1, "account_id": account_of("summarizer")})).await?;
    assert_eq!((owed["role"].as_str(), owed["amount"].as_str()), (Some("creditor"), Some("10000000")));
    Ok(())
}

// TC-SV-006-18: a bigger proposal already stored: the solver sends nothing
#[tokio::test]
async fn solver_does_not_send_a_losing_proposal() -> anyhow::Result<()> {
    if !solver_built() {
        return Ok(());
    }
    let f = s2_closed().await?;
    let c = &f.circle;
    let read_api = serve(snapshot(c).await?.to_string());
    let (solver_a, solver) = (c.worker.dev_create_account().await?, c.worker.dev_create_account().await?);
    let best = json!([
        edge("summarizer", "analyst", 15 * CREDIT),
        edge("analyst", "summarizer", 5 * CREDIT),
        edge("analyst", "coder", 10 * CREDIT),
        edge("coder", "summarizer", 10 * CREDIT)
    ]);
    assert!(c.call_as(&solver_a, "submit_clearing", json!({"epoch": 1, "edges": best})).await?.is_success());

    let run = run_solver(&f, &read_api, &solver).await?;
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("not sent"), "{}", run.stderr);
    assert_eq!(stored(c).await?["solver"].as_str(), Some(solver_a.id().as_str()));
    Ok(())
}

// TC-SV-006-20: after the proposal deadline the solver sends nothing
#[tokio::test]
async fn solver_sends_nothing_after_the_window_closes() -> anyhow::Result<()> {
    if !solver_built() {
        return Ok(());
    }
    let f = s2_closed().await?;
    let c = &f.circle;
    let read_api = serve(snapshot(c).await?.to_string());
    let solver = c.worker.dev_create_account().await?;
    c.advance_to(c.epoch_ts(1, "proposal_deadline").await?).await?;

    let run = run_solver(&f, &read_api, &solver).await?;
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("proposal window closed"), "{}", run.stderr);
    assert_eq!(stored(c).await?, Value::Null);
    Ok(())
}
