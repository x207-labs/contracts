//! TC-CT-002-04: deploy the built wasm to near-sandbox. Needs `cargo near build non-reproducible-wasm` first.
//! The contract has `PanicOnDefault` (CT-006), so before `new` (CT-008) every method refuses to run.

#[tokio::test]
async fn deploys_to_sandbox() -> anyhow::Result<()> {
    let worker = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("target/near/x207_circle.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    assert_eq!(contract.view_code().await?.len(), wasm.len());
    let err = contract
        .view("interface_version")
        .await
        .expect_err("not initialized yet");
    assert!(
        format!("{err:?}").contains("The contract is not initialized"),
        "{err:?}"
    );
    Ok(())
}
