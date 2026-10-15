# satora-ln2base

Example CLI built on the [Satora Rust SDK](https://github.com/satoraHQ/swap/tree/main/client-sdk/rust-sdk):
pay a Lightning invoice, receive USDC on Base.

```
satora-ln2base swap --to 0xYourBaseAddress --sats 50000
```

It creates a Lightning→EVM swap routed to Base, prints the BOLT11 invoice,
polls until the server has funded the Arbitrum HTLC, then claims it
automatically. The claim is a paymaster‑sponsored UserOperation on Arbitrum
whose coordinator calls swap the locked BTC token to USDC and hand it to
Circle's CCTP adapter, which mints the USDC on Base. No ETH is needed.

## Commands

| Command | What it does |
| --- | --- |
| `swap --to <addr> (--sats N \| --usdc X) [--memo ..] [--no-claim]` | Create, print invoice, wait, claim |
| `claim <swap-id>` | Resume a stored swap (wait + claim), e.g. after Ctrl‑C |
| `status <swap-id>` | Dump the server's view of a swap |
| `doctor` | Check API, bundler/paymaster config and Arbitrum RPC |

`--sats` fixes what you pay; `--usdc` fixes what arrives on Base.

Global options (also environment variables):

| Flag | Env | Default |
| --- | --- | --- |
| `--api` | `SATORA_API` | `https://api.satora.io` |
| `--mnemonic` | `SATORA_MNEMONIC` | generated into `--data-dir` on first run |
| `--data-dir` | `SATORA_DATA_DIR` | `~/.satora-ln2base` |
| `--rpc` | `ARBITRUM_RPC` | the bundler endpoint Satora serves |
| `--referral` | `SATORA_REFERRAL` | none |

The mnemonic owns the per‑swap claim keys. Back it up: a swap can only be
claimed with it, and swap records live in `swaps.sqlite` next to it.

## Build

Requires Rust 1.94. The SDK is pulled from the public
[`satoraHQ/satora-sdk`](https://github.com/satoraHQ/satora-sdk) repository.

```
cargo build --release
./target/release/satora-ln2base doctor
```

## CI and SDK updates

`.github/workflows/ci.yml` runs fmt, clippy (`-D warnings`), tests and a
release build on every push and PR. `.github/workflows/update-sdk.yml` runs
on weekdays, bumps the SDK pin to the tip of `satoraHQ/swap` main and opens a
PR; Dependabot covers GitHub Actions and crates.io dependencies.

## How the claim works

1. `GET /aa/config?chain=42161` supplies the EntryPoint, Kernel delegation
   target, bundler URL and paymaster policy.
2. `POST /dex-calldata/fund` with `to = USDC on Base` returns the coordinator
   calls: approve → Uniswap swap → approve → CCTP `bridgeBalance` → approve reset.
3. `Client::claim_evm_userop` signs the EIP‑712 redeem with the swap key,
   wraps `redeemAndExecute` in a sponsored UserOperation and submits it.
   The sweep floor is zero because the CCTP adapter drains the coordinator;
   the DEX call enforces its own 1 % minimum output.
4. The server sees the preimage, settles the hold invoice, and Circle mints
   the USDC on Base a few minutes later.

The SDK persists every submission before it leaves the process. If the CLI
dies after submitting, `claim <swap-id>` resumes from the stored UserOperation
hash instead of signing again.

## Not covered

- Refunds: a Lightning‑funded swap has no client refund; the hold invoice
  unwinds on its own if the swap fails.
- Delivery tracking on Base: the CLI stops once the server has settled the
  Lightning side. `lendaswap_sdk::bridge::BridgeApi` can follow the CCTP
  attestation if you need confirmation of the mint.
