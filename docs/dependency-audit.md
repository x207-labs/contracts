# Dependency audit, Rust part (CT-120)

Read-only audit of `contracts/Cargo.lock` (circle 0.0.16, near-sdk 5.29.1). Nothing was upgraded.

- Tool: `cargo audit` 0.22.2 (installed for this audit with `cargo install cargo-audit --locked`), RustSec advisory database of 2026-11-05 (1409 advisories). Command: `cargo audit --json` in `contracts/`.
- Reach: `cargo tree -p x207-circle --target wasm32-unknown-unknown -e normal -i <package>` for the wasm, `cargo tree -e all -i <package>` for everything else.
- The `solver` repo is TypeScript (no `Cargo.toml`), so it has no Rust audit; its npm audit is reported separately.

## Result

9 vulnerabilities and 12 warnings (unmaintained, unsound), **none of the 9 vulnerabilities and none of the unsound advisories is in the wasm that is deployed**. They reach only the host-side test tooling: `near-sdk` with the `unit-testing` feature (`near-primitives`, `near-vm-runner`, `near-parameters`) and the dev-dependency `near-workspaces` (`reqwest`, `rustls`). `cargo tree --target wasm32-unknown-unknown -e normal` shows none of `crypto-bigint`, `rustls`, `rustls-webpki`, `zstd`, `zstd-safe`, `lru`, `unsafe-libyaml`.

The only audited package inside the wasm is `wee_alloc 0.4.5` (via the default feature `wee_alloc` of `near-sdk`): RUSTSEC-2022-0054, "unmaintained". It is a notice, not a vulnerability; it is the allocator that `near-sdk` ships by default.

| Advisory | Package | Severity | In the wasm? | Where it comes from | Patched in |
|---|---|---|---|---|---|
| RUSTSEC-2026-0387 timing variability in constant-time operations | `crypto-bigint 0.5.5` | no CVSS | no | `near-crypto` through `elliptic-curve` (test tooling) | `>=0.7.6` (major jump; cannot come from `cargo update`) |
| RUSTSEC-2026-0346 `Debug` of client session values exposes TLS secrets | `rustls 0.23.43` | CVSS 3.1 AV:L/AC:H/PR:H/UI:R, confidentiality high only | no | `near-workspaces` -> `reqwest` -> `hyper-rustls` (sandbox tests download and RPC) | `>=0.23.46` |
| RUSTSEC-2026-0285 TLS 1.3 handshake messages accepted across encryption levels | `rustls 0.23.43` | CVSS 3.1 AV:N/AC:L, confidentiality low only | no | same | `>=0.23.45` |
| RUSTSEC-2026-0348 Brotli certificate decompression up to 1 GiB before authentication | `rustls 0.23.43` | CVSS 3.1 AV:N/AC:H, availability low only | no | same | `>=0.23.46` |
| RUSTSEC-2026-0349 unauthenticated ServerHello can downgrade key exchange | `rustls 0.23.43` | CVSS 3.1 AV:N/AC:H, confidentiality low only | no | same | `>=0.23.46` |
| RUSTSEC-2026-0382 certificate verification may accept distrusted intermediates | `rustls-webpki 0.103.13` | no CVSS | no | same | `>=0.103.16` |
| RUSTSEC-2026-0391 data of an abandoned stream leaks into the next | `zstd 0.13.3` | no CVSS | no | `near-primitives 0.37.4` (unit-testing feature) | `^0.13.4` |
| RUSTSEC-2026-0403 uninitialized memory on failed legacy-frame decompression | `zstd-safe 7.3.0` | no CVSS | no | same | `^7.4.0` |
| RUSTSEC-2026-0398 `Seekable::decompress` sets a `Vec` length beyond capacity | `zstd-safe 7.3.0` | no CVSS | no | same | `^7.4.0` |

Warnings (informational, none in the wasm except `wee_alloc`): `unsafe-libyaml 0.2.11` (unmaintained, via `serde_yaml` in `near-parameters`), `lru 0.16.4` (RUSTSEC-2026-0253, unsound `pop()`, via `near-vm-runner`), nine more unsound advisories on `zstd-safe 7.3.0` (RUSTSEC-2026-0395 to 0406, same path as above), `wee_alloc 0.4.5` (in the wasm, see above).

## Why this matters little for the circle

The contract never parses TLS, never decompresses zstd and does no big-integer arithmetic of its own: signatures use the host functions (`env::ed25519_verify`, `env::ecrecover`, `env::keccak256`). The advisories sit in code that runs on a developer machine or in CI while the tests run, and that talks only to a local sandbox or to the public RPC.

## Fixes

Status after the update of 2026-11-05 (contracts `4088b6f`):

1. **Applied, zstd:** `cargo update -p rustls -p rustls-webpki -p zstd -p zstd-safe` moved only `zstd 0.13.3 -> 0.13.4` and `zstd-safe 7.3.0 -> 7.4.0` (closes RUSTSEC-2026-0391, 0403, 0398 and the nine unsound advisories on `zstd-safe`). `Cargo.lock` changed, `Cargo.toml` did not. The reproducible build of the commit after the update (`scripts/repro-build.sh 4088b6f`) gives wasm sha256 `4295351f395d0cc31419d43adf10e5b60f3c2a0e546ae0e912c5da4fef5aebe7`, the same as v0.0.16, as expected because none of these packages is in the wasm. The ABI freeze and init tests pass.
1b. **Not possible now, rustls and rustls-webpki:** the patched `rustls 0.23.46` needs `aws-lc-rs ^1.18`, while `near-crypto 0.37.4` (through `near-sdk 5.29.1`) pins `aws-lc-rs =1.16.2`, so `cargo update -p rustls --precise 0.23.46` fails with "failed to select a version for `aws-lc-rs`". The same holds for `rustls-webpki 0.103.16`. It needs a `near-sdk` / `near-crypto` release; the packages are only in the test tooling (`near-workspaces` -> `reqwest`), not in the wasm. Known risk, no action now.
2. `crypto-bigint 0.5.5` needs a new `near-crypto` / `near-sdk`; wait for a `near-sdk` release, no action now.
3. `wee_alloc`: nothing to do while `near-sdk` makes it the default; switching the allocator would change the wasm hash and is not worth a release.
4. **Applied (CT-121):** `.github/workflows/audit.yml` runs `cargo audit` on every push, pull request and weekly; the job is non-blocking (`continue-on-error: true`). The advisories of this page are listed with their reasons in `.cargo/audit.toml`, so a new advisory is the only thing that turns the step red. With that list `cargo audit` reports 0 vulnerabilities and no warnings on `Cargo.lock` of 2026-11-05.
