# Contract Verification

## Does the deployed contract match my build?

```sh
soroban-forge verify CDLZ… --network testnet
```

`verify` compares the wasm deployed at a contract ID with your local release
build. A Soroban contract's on-chain wasm hash is the SHA-256 of its deployed
bytes, so the check is a hash comparison:

| side       | what is hashed                                                  |
|------------|-----------------------------------------------------------------|
| local      | `target/wasm32v1-none/release/<crate>.wasm`, or `--wasm <path>` |
| on-chain   | the bytes `stellar contract fetch` returns for the contract ID  |

A match means the deployed contract is byte-for-byte the wasm you have
locally.

```
✓ verified — the deployed contract matches the local build

  contract   CDLZ…
  network    testnet
  local      target/wasm32v1-none/release/my_token.wasm

  sha256     9f2c…
```

A mismatch names both hashes and summarises how the two interfaces differ,
so you can tell at a glance whether the deployment is missing an
entrypoint, has an old signature, or only differs in implementation:

```
✗ MISMATCH — the deployed contract was NOT built from this wasm

  contract   CDLZ…
  network    testnet
  local      target/wasm32v1-none/release/my_token.wasm

  local      sha256 9f2c…
  on-chain   sha256 41ab…

  interface changes (on-chain → local):
    + mint(to: address, amount: i128)
    - burn(from: address, amount: i128)
    ~ balance
        on-chain  balance(id: address) -> i128
        local     balance(id: address) -> u128
```

`+` is an entrypoint only the local build has, `-` one only the deployment
has, and `~` one whose arguments or return type changed. Both interfaces
are read with `stellar contract info interface` from the two wasm files
`verify` already has on disk, so the diff costs no extra network call.

When the entrypoints are identical the report says `interface unchanged`,
which points at the implementation or build settings rather than the API.
If either interface cannot be read (for example the deployed wasm has no
`contractspecv0` section) the report prints
`interface diff unavailable: <reason>` and the mismatch verdict and exit
code are unchanged.

### Options

| flag                       | meaning                                                     |
|----------------------------|-------------------------------------------------------------|
| `--path <dir>`             | contract project directory [default: current directory]     |
| `--wasm <file>`            | compare this wasm instead of the project's release build     |
| `--network`, `-n <name>`   | configured network to query [default: `testnet`]            |
| `--rpc-url <url>`          | query this RPC endpoint instead of a configured network      |
| `--network-passphrase <p>` | passphrase for `--rpc-url`                                   |
| `--reproducible`           | build inside the pinned image before hashing                 |

`--wasm` and `--reproducible` are mutually exclusive — the former skips
the local build entirely (useful for verifying a downloaded release
artifact), the latter rebuilds from source inside the pinned container
so the result does not depend on the local toolchain.

When the network is not given on the command line it falls back to
`[network]` in `forge.toml`:

```toml
[network]
name = "testnet"
rpc_url = "https://soroban-testnet.stellar.org"
passphrase = "Test SDF Network ; September 2015"
```

CLI flags override the file values; `--network` and `--rpc-url` may be
combined to point a named network at a non-default RPC endpoint.

### Reproducible builds

`--reproducible` runs `stellar contract build` inside the image pinned at
[`REPRODUCIBLE_IMAGE`] in `crates/verify/src/lib.rs` and hashes the
result. The image is pinned by digest (`@sha256:…`), so the bytes
produced for a given source tree do not depend on the host toolchain —
useful when verifying a release against the original sources.

The pin is the official `stellar/stellar-cli` image, which ships the
`stellar` binary and the `wasm32v1-none` target soroban-forge builds
against:

```
docker.io/stellar/stellar-cli@sha256:daed639814911ebcafe181027fe40ab24fa6b5fe24ce0b388f95f1f38f9856ff
```

The contract directory is bind-mounted at `/src` (writable — the build
writes `target/`, which is also how the wasm reaches the host to be
hashed) and the entrypoint is set explicitly to `stellar`, because the
image already entrypoints there.

A missing Docker daemon is reported as exit `2` (missing tool) so CI can
distinguish it from a real mismatch:

```
$ soroban-forge verify --reproducible CDLZ…
error: docker not found on PATH (run `soroban-forge doctor` for install instructions)
```

To refresh the pin, resolve the digest of the release you want and update
the `REPRODUCIBLE_IMAGE` constant to it:

```sh
docker buildx imagetools inspect stellar/stellar-cli:26.1.0 --format '{{.Manifest.Digest}}'
```

Then rebuild; the comparison only succeeds with the exact image that
produced the bytes in the first place. A unit test rejects the all-zero
placeholder digest the constant used to carry, so an accidental revert
fails in CI rather than at `docker run`.

### Exit codes

| code | meaning                                                        |
|------|-----------------------------------------------------------------|
| `0`  | the hashes match                                                |
| `1`  | the hashes differ, or an argument/local build was wrong         |
| `2`  | the `stellar` CLI is not installed (`soroban-forge doctor`)     |

So a CI job can gate on a deployment being the reviewed code:

```sh
soroban-forge verify "$CONTRACT_ID" --network testnet || exit 1
```

`--json` prints the same report machine-readably, which distinguishes a
mismatch from a bad argument (both exit `1`):

```json
{
  "contract_id": "CDLZ…",
  "network": "testnet",
  "local_wasm": "target/wasm32v1-none/release/my_token.wasm",
  "local_hash": "9f2c…",
  "onchain_hash": "41ab…",
  "match": false,
  "spec_diff": {
    "status": "available",
    "added": ["mint(to: address, amount: i128)"],
    "removed": ["burn(from: address, amount: i128)"],
    "changed": [
      {
        "name": "balance",
        "onchain": "balance(id: address) -> i128",
        "local": "balance(id: address) -> u128"
      }
    ]
  }
}
```

`spec_diff` is only present on a mismatch. When the interfaces cannot be
read it is `{"status": "unavailable", "reason": "…"}` instead.

### When a mismatch is expected

- the deployed contract is an **older build** — redeploy, or check out the
  commit that was deployed and rebuild
- you built with **different flags** than the deployment (wasm is sensitive
  to optimisation settings; always compare a `stellar contract build` release
  artifact, not a `cargo build` one)
- you are pointed at the **wrong network** — `--network mainnet` and
  `--network testnet` hold different deployments of the same project

## Public source verification

Publishing your *source* alongside the deployment — so explorers such as
Stellar Expert can show a verified badge and users can read the code their
funds interact with — is a separate flow run through the Stellar contract
verification service, and is not part of `soroban-forge` today. `verify`
answers the local question only: does this build correspond to that
deployment?
