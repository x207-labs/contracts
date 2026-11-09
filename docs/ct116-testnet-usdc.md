# CT-116: real USDC on NEAR testnet for the circle, or mock USDT

Date: 2026-11-09. Author: Worker F. Status: decision note with evidence; no circle was redeployed and no real USDC was staked or settled (see "What is not done").

## Decision

Use the real testnet USDC of Circle for the demo circle that judges and users see, at the next fresh deployment of that circle. Keep the mock USDT (`usdt.x207-deployer.testnet`, `probes/mock_ft`) for rehearsals, stress and epoch tests. No contract change is needed: the token of a circle is the init argument `usdt_token_id` (`x207-circle/src/config.rs`), there is no setter, so switching a circle to USDC means a new circle deployment. That needs the owner's approval and was not done here.

## The token

- Account: `3e2210e1184b45b64c8a434c0a7e7b23cc04ea7eb7a6c3c32520d03d4afcb8af` (implicit account, 64 hex characters).
- Source of the address: the near-kit crate documents it as the testnet address of USDC (docs.rs, near-kit 0.7.2, `src/tokens/known.rs`, the constant `USDC`). The USDC-on-NEAR page of Circle (circle.com/multi-chain-usdc/near) lists a testnet address of the form `3e22...b8af` with token standard NEP-141, which matches the first and last characters. The Circle pages could not be opened from this machine (TLS certificate error for developers.circle.com in the browser, no response over curl), so the full address was not read from a Circle page. The on-chain facts below are what confirm it is the token.
- On-chain facts, read through the RPC `https://rpc.testnet.fastnear.com` on 2026-11-09 (final block, height around 276927839):
  - `ft_metadata`: spec `ft-1.0.0`, name `USDC`, symbol `USDC`, decimals 6 (the same as the credit unit of the circle, 1 credit = 1,000,000 units).
  - `ft_total_supply`: 14081220210567770 units.
  - Code hash `EoAUFWELjD9gYzsjKDXYRmHDHf1RJ95jaKNSYE6v9b2z`, storage usage 3,259,618 bytes.
  - The panic messages of the contract name its source file `src/fiat_token.rs`, which fits the fiat-token contract of Circle.
  - `ft_transfer_call` exists (it asks for `receiver_id`), so the circle can receive stake and settle payments through `ft_on_transfer` exactly as it does with the mock.
  - `storage_balance_bounds`: min and max both 0.00125 NEAR, a fixed deposit (the same as the mock; `storage_deposit` is needed once per account, and the circle itself needs one before it can receive USDC).
  - `mint` exists but takes a `to` field and is for minters only (`is_minter` of an ordinary account is false); there is no public mint.
  - There is one full-access key on the account, held by whoever controls the contract (Circle). The contract can be changed by that party at any time.
- How to get test USDC: the public Circle faucet (faucet.circle.com) lists NEAR testnet and gives 10 USDC per request, one request per 24 hours per token per testnet, to any address, without an account. This comes from a web search of the Circle documentation, not from the faucet page itself, so check the limits on the page. The Developer Console faucet needs a Circle account.

## Why the mock stays for tests

- Real USDC cannot be minted by us, and the faucet gives 10 USDC per address per day. One epoch rehearsal in this project uses 150 USDT of stake per member plus the voucher payments (CT-127 and CT-131 minted 300 units per member). With the faucet this takes many addresses and many days, and it needs a manual browser step, so it cannot be scripted into the rehearsal runs.
- The mock has a free `mint`, which the keeper, the clearing runs and the CI rely on.

## Why real USDC for the demo circle

- It shows that the contract works with the token that is meant to be used and not only with a mock the team controls.
- The interface is plain NEP-141 with `ft_transfer_call`, `storage_deposit` of a fixed amount and 6 decimals; nothing in `settlement.rs`, `stake.rs` or `commit.rs` depends on anything the mock adds, so no code or ABI change is expected.

## Risks

- The token is controlled by Circle: they can pause, upgrade or reset it on testnet and the supply is not under our control. A demo can break for reasons outside the project. Mitigation: keep a mock circle ready and keep the demo script able to point at either.
- The faucet limit (10 USDC per day per address) caps how much stake and settlement the demo can show; the stake of a member in the rehearsals (150 units) needs 15 faucet requests from one address, so the demo uses small amounts or several addresses. The `bond_threshold` and the stake rules of the demo circle may need to be set low (`set_params`) before the demo; check them first (not checked here).
- The circle must be registered at the token (`storage_deposit` of 0.00125 NEAR) before the first stake, and every claimant and relayer needs it too; the claim of a payout to an account without storage fails at the token, and the circle then keeps the position claimable (that behaviour is already tested with the mock for the relayer fee).
- The address is from a third-party crate plus the matching ends on the Circle page. If Circle publishes another testnet address later, the on-chain check above must be repeated.

## Migration steps (not executed)

1. Owner approves a new circle deployment on testnet (a new account; do not reuse `x207-v018.testnet`, which is the build under test).
2. Deploy the committed `prebuilt/x207_circle-0.0.18.wasm` (sha256 `4701ecce4168d18b0674b6b4af9f789219afc33be99d89c09f3a433c0bd0d198`) with `new` and `usdt_token_id` set to the USDC account above (the other init arguments as in `scripts/deploy-v018-testnet.mjs`, which deploys a circle with `new` including `usdt_token_id`).
3. `storage_deposit` of the circle and of each test account at the USDC contract (0.00125 NEAR each).
4. Fund the member accounts from the Circle faucet (10 USDC per address per 24 hours) and set low stake and bond parameters through `set_params`.
5. Run one stake and one settle with real USDC and record the transaction hashes in `docs/testnet-epochs/`; only then is the backlog line CT-116 closed with "one stake and one settle with real USDC".
6. Point the backend keeper and the SDK demo configuration at the new circle and token. The SDK contract client takes the token as the optional field `usdtTokenId` and falls back to a constant `USDT_TOKEN_ID` (`packages/core/src/contract/client.ts`); whether every SDK and backend path reads the token from configuration and not from that constant was not checked here.

## What is not done

- No real USDC was staked or settled (no USDC on any of our accounts, and the faucet needs a manual browser step). The line of the backlog that asks for a stake and a settle with real USDC stays open; this note is the written decision and the prepared migration.
- The pause, blacklist and upgrade behaviour of the Circle contract was not tested; the views for them were probed and do not exist under the names tried.
- The address was not read from a Circle page (see above).
