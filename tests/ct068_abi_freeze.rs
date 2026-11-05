//! CT-068: the frozen interface. `abi.json` at the repo root is the file `sdk`, `backend`, and
//! `solver` read. It is the cargo-near ABI (without doc strings and build metadata) plus the event
//! list of `events.rs`.
//!
//! Any change visible from outside (method, argument, return type, event field) must bump the
//! version: `version` in `Cargo.toml` and `INTERFACE_VERSION` in `lib.rs` (spec section 1, rule 1),
//! then regenerate and announce to `sdk`, `backend`, `solver`:
//!
//!     cargo near build non-reproducible-wasm
//!     UPDATE_ABI=1 cargo test --test ct068_abi_freeze committed_abi_matches_build

use serde_json::{json, Map, Value};
use x207_circle::events::EVENT_SCHEMAS;
use x207_circle::INTERFACE_VERSION;

const BUILT_ABI: &str = "target/near/x207_circle_abi.json";
const COMMITTED_ABI: &str = "abi.json";

/// Methods that change state (spec sections 4 and 14: P0, and the P1 methods that joined with a patch bump, K-20).
const CHANGE_METHODS: [&str; 50] = [
    "new", "join", "ft_on_transfer", "register_cumulative", "register_voucher", "close_epoch", "submit_clearing",
    "commit_epoch", "finalize_epoch", "claim", "on_claim_resolved", "set_params", "request_exit", "withdraw_stake",
    "on_withdraw_resolved",
    // 0.0.2: CT-018 and CT-052; 0.0.3: CT-055 (`migrate` is called on the new code by `apply_upgrade`)
    "admit_member", "claim_solver_fee", "on_solver_fee_resolved", "propose_upgrade", "apply_upgrade", "migrate",
    // 0.0.4: CT-062, CT-063, CT-064 (F5 barter bundle)
    "post_intent", "commit_bundle", "register_in_bundle",
    // 0.0.6: F8 (CT-069)
    "set_asset_class",
    // CT-070
    "offer_grant", "accept_grant",
    // CT-071
    "revoke_grant",
    // 0.0.7: CT-075 (class A escrow)
    "nft_on_transfer", "complete_escrow", "cancel_escrow", "on_escrow_resolved",
    // 0.0.8: security review F2 and CT-085 (prune_epoch)
    "remove_intent", "prune_epoch",
    // 0.0.9: BE-034 (NEP-245 receiver)
    "mt_on_transfer", "withdraw_intents_balance", "on_intents_withdrawn",
    // 0.0.12: CT-095 (freeze_member)
    "propose_freeze", "apply_freeze", "cancel_freeze", "unfreeze_member",
    // 0.0.12: CT-096 (official contracts)
    "set_asset_contracts",
    // 0.0.12: CT-094 (bond with an arbiter)
    "confirm_deal", "dispute_deal", "rule_deal", "settle_deal",
    // 0.0.14: CT-104 and CT-105 (EVM agents)
    "join_evm", "claim_evm", "request_exit_evm", "withdraw_stake_evm",
];

/// The 19 P0 views of spec section 12, `solver_fee_balance` (CT-052, 0.0.2) and `get_upgrade` (CT-055, 0.0.3).
const VIEWS: [&str; 38] = [
    "interface_version", "get_circle", "get_owner", "get_params", "get_pending_params", "member", "list_members",
    "limit", "headroom", "outstanding_debt", "last_seq", "obligation", "list_obligations", "get_epoch",
    "simulate_clearing", "get_clearing_proposal", "net_position", "list_positions", "get_pool", "solver_fee_balance", "get_upgrade", "get_bundle", "get_intent",
    "get_asset_class", "list_asset_classes", "get_grant", "grants_of", "get_escrow", "get_ft_escrow", "get_mt_escrow", "intents_balance", "get_freeze", "is_frozen", "get_asset_contracts", "get_deal", "bond_locked", "get_proposal", "evm_signer",
];

const P1_P2_METHODS: [&str; 0] = [];

/// Final signatures. `string` also stands for `AccountId`, `U128`, `U64`, `PublicKey`, which the ABI
/// does not tell apart. `[..]` lists the modifiers.
const SIGNATURES: [&str; 88] = [
    "accept_grant(grant_id:integer:uint64,delivery_pubkey:string|null)-> []",
    "admit_member(account_id:string)-> []",
    "apply_freeze(account_id:string)-> []",
    "apply_upgrade()->Promise []",
    "bond_locked(account_id:string)->string []",
    "cancel_escrow(grant_id:integer:uint64)->Promise []",
    "cancel_freeze(account_id:string)-> []",
    "claim(epoch:integer:uint64,destination:ClaimDestination)->Promise [payable]",
    "claim_evm(member:string,epoch:integer:uint64,receiver:string,nonce:integer:uint64,deadline:integer:uint64,signature:string)->Promise []",
    "claim_solver_fee()->Promise [payable]",
    "close_epoch(epoch:integer:uint64)-> []",
    "commit_bundle(bundle:BundleInput,signatures:array<BundleSignature>)->integer:uint64 []",
    "commit_epoch(epoch:integer:uint64)-> []",
    "complete_escrow(grant_id:integer:uint64,voucher:Voucher,signature:string)->Promise []",
    "confirm_deal(bundle_id:integer:uint64)-> []",
    "dispute_deal(bundle_id:integer:uint64,evidence_hash:string)-> []",
    "evm_signer(account_id:string)->EvmSignerView|null []",
    "finalize_epoch(epoch:integer:uint64)-> []",
    "ft_on_transfer(sender_id:string,amount:string,msg:string)->PromiseOrValueString []",
    "get_asset_class(asset_type:string)->AssetClassEntry|null []",
    "get_asset_contracts(asset_type:string)->array<string> []",
    "get_bundle(bundle_id:integer:uint64)->BundleView|null []",
    "get_circle()->CircleConfig []",
    "get_clearing_proposal(epoch:integer:uint64)->ClearingProposal|null []",
    "get_deal(bundle_id:integer:uint64)->DealView|null []",
    "get_epoch(epoch:integer:uint64|null)->EpochView|null []",
    "get_escrow(grant_id:integer:uint64)->EscrowView|null []",
    "get_freeze(account_id:string)->FreezeView|null []",
    "get_ft_escrow(grant_id:integer:uint64)->FtEscrowView|null []",
    "get_grant(grant_id:integer:uint64)->AccessGrant|null []",
    "get_intent(intent_hash:string)->IntentView|null []",
    "get_mt_escrow(grant_id:integer:uint64)->MtEscrowView|null []",
    "get_owner()->string []",
    "get_params()->Params []",
    "get_pending_params()->PendingParams|null []",
    "get_pool()->PoolView []",
    "get_proposal(epoch:integer:uint64,from_index:integer:uint32,limit:integer:uint32)->ProposalView|null []",
    "get_upgrade()->UpgradeView|null []",
    "grants_of(account_id:string,role:GrantRole,from_index:integer:uint32,limit:integer:uint32)->array<AccessGrant> []",
    "headroom(account_id:string)->string []",
    "intents_balance()->string []",
    "interface_version()->string []",
    "is_frozen(account_id:string)->boolean []",
    "join(circle_id:string,pubkey_voucher:string,terms_hash:string)->MemberView [payable]",
    "join_evm(address:string,terms_hash:string,deadline:integer:uint64,signature:string)->MemberView [payable]",
    "last_seq(payer:string,payee:string,epoch:integer:uint64)->integer:uint64 []",
    "limit(account_id:string)->string []",
    "list_asset_classes(from_index:integer:uint32,limit:integer:uint32)->array<AssetClassEntry> []",
    "list_members(from_index:integer:uint32,limit:integer:uint32)->array<MemberView> []",
    "list_obligations(epoch:integer:uint64,from_index:integer:uint32,limit:integer:uint32)->array<ObligationView> []",
    "list_positions(epoch:integer:uint64,from_index:integer:uint32,limit:integer:uint32)->array<PositionView> []",
    "member(account_id:string)->MemberView|null []",
    "migrate()-> [init,private]",
    "mt_on_transfer(sender_id:string,previous_owner_ids:array<string>,token_ids:array<string>,amounts:array<string>,msg:string)->PromiseOrValueArray_of_String []",
    "net_position(epoch:integer:uint64,account_id:string)->PositionView|null []",
    "new(owner_id:string,circle_id:string,usdt_token_id:string,intents_id:string,params:Params|null)-> [init]",
    "nft_on_transfer(sender_id:string,previous_owner_id:string,token_id:string,msg:string)->PromiseOrValueBoolean []",
    "obligation(epoch:integer:uint64,debtor:string,creditor:string)->ObligationView|null []",
    "offer_grant(input:GrantInput)->integer:uint64 []",
    "on_claim_resolved(epoch:integer:uint64,account_id:string,amount:string,destination:ClaimDestination)->string [private]",
    "on_escrow_resolved(grant_id:integer:uint64,step:EscrowStep)-> [private]",
    "on_intents_withdrawn(amount:string)->string [private]",
    "on_solver_fee_resolved(account_id:string,amount:string)->string [private]",
    "on_withdraw_resolved(account_id:string,amount:string)->boolean [private]",
    "outstanding_debt(account_id:string)->string []",
    "post_intent(intent_hash:string)-> []",
    "propose_freeze(account_id:string,reason_hash:string|null)-> []",
    "propose_upgrade(code_hash:string)-> []",
    "prune_epoch(epoch:integer:uint64,max_items:integer:uint32)->integer:uint32 []",
    "register_cumulative(voucher:Voucher,signature:string)->string []",
    "register_in_bundle(bundle_id:integer:uint64,voucher:Voucher,signature:string)->string []",
    "register_voucher(voucher:Voucher,signature:string)->string []",
    "remove_intent(intent_hash:string)-> []",
    "request_exit()-> [payable]",
    "request_exit_evm(member:string,nonce:integer:uint64,deadline:integer:uint64,signature:string)-> []",
    "revoke_grant(grant_id:integer:uint64,reason:RevokeReason)-> []",
    "rule_deal(bundle_id:integer:uint64,verdict:Verdict)-> []",
    "set_asset_class(asset_type:string,entry:AssetClassEntry)-> []",
    "set_asset_contracts(asset_type:string,contracts:array<string>)-> []",
    "set_params(params:Params)-> []",
    "settle_deal(bundle_id:integer:uint64)-> []",
    "simulate_clearing(epoch:integer:uint64,edges:array<ClearingEdge>)->SimulationResult []",
    "solver_fee_balance(account_id:string)->string []",
    "submit_clearing(epoch:integer:uint64,edges:array<ClearingEdge>)->string []",
    "unfreeze_member(account_id:string)-> []",
    "withdraw_intents_balance()->Promise []",
    "withdraw_stake()->PromiseOrValueBoolean [payable]",
    "withdraw_stake_evm(member:string,receiver:string,nonce:integer:uint64,deadline:integer:uint64,signature:string)->PromiseOrValueBoolean []",
];

const EVENTS: [&str; 46] = [
    "member_joined", "stake_added", "voucher_registered", "epoch_cutoff", "clearing_submitted", "clearing_applied", "epoch_committed", "residual_paid", "member_defaulted", "limits_updated", "epoch_finalized", "residual_claimed", "residual_claim_failed", "params_updated", "exit_requested", "stake_withdrawn", "stake_withdraw_failed", "member_admitted", "member_reactivated", "solver_fee_claimed", "solver_fee_claim_failed", "upgrade_proposed", "upgrade_applied", "intent_posted", "asset_class_set", "grant_offered", "grant_activated", "grant_revoked", "escrow_held", "ft_escrow_held", "freeze_proposed", "member_frozen", "freeze_cancelled", "member_unfrozen", "asset_contracts_set", "bond_locked", "deal_confirmed", "deal_disputed", "deal_ruled", "bond_slashed", "bond_released", "mt_escrow_held", "escrow_failed", "bundle_committed", "intents_withdrawn", "intent_removed",
];

fn strip_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            map.remove("doc");
            map.values_mut().for_each(strip_descriptions);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        _ => {}
    }
}

/// Orders object keys, so the file does not depend on the key order of the build (it changes with
/// the `serde_json` features that other dependencies switch on).
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// The document committed as `abi.json`.
fn build_document() -> Value {
    let built: Value = serde_json::from_str(&std::fs::read_to_string(BUILT_ABI).expect("run cargo near build first")).unwrap();
    let mut functions: Vec<Value> = built["body"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["name"] != "contract_source_metadata")
        .cloned()
        .collect();
    functions.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let events: Vec<Value> = EVENT_SCHEMAS
        .iter()
        .map(|e| {
            let mut event = Map::new();
            event.insert("name".into(), json!(e.name));
            event.insert("fields".into(), json!(e.fields));
            if !e.items.is_empty() {
                event.insert("list_item_fields".into(), json!(e.items));
            }
            Value::Object(event)
        })
        .collect();
    let mut document = json!({
        "interface_version": INTERFACE_VERSION,
        "schema_version": built["schema_version"],
        "metadata": {"name": built["metadata"]["name"], "version": built["metadata"]["version"]},
        "event_standard": "x207",
        "events": events,
        "body": {"functions": functions, "root_schema": built["body"]["root_schema"]},
    });
    strip_descriptions(&mut document);
    sort_keys(document)
}

fn schema_name(schema: &Value) -> String {
    if let Some(reference) = schema.get("$ref") {
        return reference.as_str().unwrap().rsplit('/').next().unwrap().to_string();
    }
    if let Some(parts) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
        let mut names: Vec<String> = parts.as_array().unwrap().iter().map(schema_name).collect();
        names.sort();
        return names.join("|");
    }
    let format = schema.get("format").and_then(Value::as_str).map(|f| format!(":{f}")).unwrap_or_default();
    match &schema["type"] {
        Value::Array(types) => {
            let base: Vec<&str> = types.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect();
            format!("{}{format}|null", base.join("|"))
        }
        Value::String(t) if t == "array" => format!("array<{}>", schema_name(&schema["items"])),
        Value::String(t) => format!("{t}{format}"),
        _ => "?".to_string(),
    }
}

fn signature(function: &Value) -> String {
    let args: Vec<String> = function["params"]["args"]
        .as_array()
        .map(|args| args.iter().map(|a| format!("{}:{}", a["name"].as_str().unwrap(), schema_name(&a["type_schema"]))).collect())
        .unwrap_or_default();
    let result = function.get("result").map(|r| schema_name(&r["type_schema"])).unwrap_or_default();
    let modifiers: Vec<&str> = function["modifiers"].as_array().map(|m| m.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    format!("{}({})->{result} [{}]", function["name"].as_str().unwrap(), args.join(","), modifiers.join(","))
}

fn functions(document: &Value) -> Vec<Value> {
    document["body"]["functions"].as_array().unwrap().clone()
}

fn names(document: &Value, kind: &str) -> Vec<String> {
    functions(document).iter().filter(|f| f["kind"] == kind).map(|f| f["name"].as_str().unwrap().to_string()).collect()
}

fn sorted(items: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

// TC-CT-068-01
#[test]
fn all_p0_change_methods_exist_and_p1_p2_do_not() {
    let document = build_document();
    let calls = names(&document, "call");
    assert_eq!(calls, sorted(&CHANGE_METHODS), "change methods differ from spec sections 4 and 14");
    for name in P1_P2_METHODS {
        assert!(!names(&document, "call").contains(&name.to_string()) && !names(&document, "view").contains(&name.to_string()), "{name} is P1/P2 and must not be in the ABI yet");
    }
}

// TC-CT-068-02
#[test]
fn all_19_p0_views_exist() {
    let document = build_document();
    assert_eq!(names(&document, "view"), sorted(&VIEWS));
    for name in ["get_upgrade_history"] {
        assert!(!names(&document, "view").contains(&name.to_string()), "{name}");
    }
}

// TC-CT-068-03, TC-CT-068-04
#[test]
fn signatures_modifiers_and_returns_match_the_spec() {
    let document = build_document();
    let actual: Vec<String> = functions(&document).iter().map(signature).collect();
    assert_eq!(actual, SIGNATURES.map(String::from).to_vec());

    // The voucher keeps camelCase on the wire, as in the x402 payload.
    let voucher = &document["body"]["root_schema"]["definitions"]["Voucher"];
    let properties: Vec<&str> = voucher["properties"].as_object().unwrap().keys().map(String::as_str).collect();
    assert!(properties.contains(&"cumulativeAmount") && !properties.contains(&"cumulative_amount"));
    assert_eq!(voucher["properties"]["cumulativeAmount"]["type"], "string");
}

// TC-CT-068-05
#[tokio::test]
async fn version_is_consistent_everywhere() -> anyhow::Result<()> {
    let document = build_document();
    assert_eq!(document["metadata"]["version"], INTERFACE_VERSION);
    assert_eq!(document["interface_version"], INTERFACE_VERSION);
    assert_eq!(env!("CARGO_PKG_VERSION"), INTERFACE_VERSION);

    let worker = near_workspaces::sandbox().await?;
    let contract = worker.dev_deploy(&std::fs::read("target/near/x207_circle.wasm")?).await?;
    contract
        .call("new")
        .args_json(json!({"owner_id": "dao.test.near", "circle_id": "agents-main", "usdt_token_id": "usdt.test.near", "intents_id": "intents.test.near", "params": null}))
        .transact()
        .await?
        .into_result()?;
    let view: String = contract.view("interface_version").await?.json()?;
    assert_eq!(view, INTERFACE_VERSION);
    Ok(())
}

// TC-CT-068-06
#[test]
fn committed_abi_matches_build() {
    let document = build_document();
    let text = format!("{}\n", serde_json::to_string_pretty(&document).unwrap());
    if std::env::var("UPDATE_ABI").is_ok() {
        std::fs::write(COMMITTED_ABI, &text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(COMMITTED_ABI).expect("abi.json is committed at the repo root");
    assert!(
        committed == text,
        "the interface changed without a new abi.json. Bump the version (Cargo.toml and INTERFACE_VERSION), regenerate with \
         `UPDATE_ABI=1 cargo test --test ct068_abi_freeze committed_abi_matches_build`, and announce it to sdk, backend, solver"
    );
}

// TC-CT-068-07
#[test]
fn committed_event_list_is_frozen() {
    let committed: Value = serde_json::from_str(&std::fs::read_to_string(COMMITTED_ABI).unwrap()).unwrap();
    let events: Vec<&str> = committed["events"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(events, EVENTS);
    for event in committed["events"].as_array().unwrap() {
        assert!(!event["fields"].as_array().unwrap().is_empty());
    }
    assert_eq!(committed["event_standard"], "x207");
}
