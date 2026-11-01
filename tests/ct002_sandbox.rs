//! TC-CT-002-04: deploy the built wasm to near-sandbox. Needs `cargo near build non-reproducible-wasm` first.

#[tokio::test]
async fn deploys_to_sandbox() -> anyhow::Result<()> {
    let worker = near_workspaces::sandbox().await?;
    let wasm = std::fs::read("target/near/x207_circle.wasm")?;
    let contract = worker.dev_deploy(&wasm).await?;
    let version: String = contract.view("interface_version").await?.json()?;
    assert_eq!(version, "0.0.1");
    Ok(())
}
