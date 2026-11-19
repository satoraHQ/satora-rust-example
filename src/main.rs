//! Lightning → USDC on Base, end to end.
//!
//! 1. Creates a Lightning→EVM swap on Satora whose payout is USDC on Base.
//!    The HTLC lives on the Arbitrum hub; the daemon bridges the DEX output
//!    to Base through Circle CCTP as part of the claim.
//! 2. Prints the BOLT11 invoice to pay.
//! 3. Polls the swap and, once the server has funded the Arbitrum HTLC,
//!    claims it through a paymaster-sponsored UserOperation (the only
//!    working path for Arbitrum DEX/CCTP payouts, same as the TypeScript SDK).
//!
//! Swap records are persisted in SQLite so an interrupted claim can be
//! resumed with `claim <swap-id>`.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use lendaswap_sdk::aa::bundler::BundlerCasing;
use lendaswap_sdk::aa::{AaConfig, PaymasterConfig, UserOpConfig};
use lendaswap_sdk::evm::{Address, EvmClaimOptions, EvmWallet, RpcWallet, U256};
use lendaswap_sdk::types::{
    BridgeRecipient, BridgeRoute, Chain, DexAmount, DexCalldataRequest, DexCallsPayload, DexToken,
    EvmAsset, GetSwapResponse, KnownChain, LightningToEvmSwapResponse, QuoteAmount, QuoteRequest,
    SwapCreation, SwapOptions, SwapStatus, TokenId,
};
use lendaswap_sdk::{Client, Endpoint, PayloadKind, Signer, StoredSwap};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const ARBITRUM: u64 = 42161;
const BASE: u64 = 8453;
/// Native USDC on Base (Circle).
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
/// Server-side name of the CCTP destination (`bridge_target_chain`).
const BRIDGE_CHAIN_BASE: &str = "Base";
/// Slippage the daemon applies to Lightning→EVM DEX legs.
const DEX_SLIPPAGE_BPS: u16 = 100;

#[derive(Parser)]
#[command(
    name = "satora-ln2base",
    version,
    about = "Swap Lightning sats to USDC on Base via Satora"
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Global {
    /// Satora API base URL.
    #[arg(
        long,
        env = "SATORA_API",
        default_value = "https://api.satora.io",
        global = true
    )]
    api: String,
    /// BIP39 mnemonic. If omitted, a wallet is created/loaded from `--data-dir`.
    #[arg(long, env = "SATORA_MNEMONIC", hide_env_values = true, global = true)]
    mnemonic: Option<String>,
    /// Directory for wallet.sqlite and swaps.sqlite.
    #[arg(long, env = "SATORA_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,
    /// Arbitrum JSON-RPC URL used for chain reads and the claim wallet.
    /// Defaults to the bundler endpoint Satora serves (Alchemy serves eth_* too).
    #[arg(long, env = "ARBITRUM_RPC", global = true)]
    rpc: Option<Url>,
    /// Optional referral code.
    #[arg(long, env = "SATORA_REFERRAL", global = true)]
    referral: Option<String>,
    /// Verbose SDK logging (RUST_LOG also works).
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Quote sats → USDC on Base without creating anything (no wallet needed).
    Quote(QuoteArgs),
    /// Create the swap, print the invoice, and claim once the server funds.
    Swap(SwapArgs),
    /// Resume/claim an existing swap by ID (e.g. after an interruption).
    Claim {
        /// Swap ID returned by `swap`.
        swap_id: String,
    },
    /// Print the status of a stored swap.
    Status { swap_id: String },
    /// Check API, AA bundler/paymaster config and Arbitrum RPC without swapping.
    Doctor,
}

#[derive(Args)]
struct SwapArgs {
    /// Base address that receives the USDC.
    #[arg(long)]
    to: String,
    /// Amount to pay over Lightning, in sats (fees come out of the USDC payout).
    #[arg(long, conflicts_with = "usdc")]
    sats: Option<u64>,
    /// Exact USDC amount to receive on Base, e.g. 25 or 12.50 (invoice covers fees).
    #[arg(long, conflicts_with = "sats")]
    usdc: Option<String>,
    /// Invoice description (max 639 bytes).
    #[arg(long)]
    memo: Option<String>,
    /// Only create and print the invoice; do not wait or claim.
    #[arg(long)]
    no_claim: bool,
}

#[derive(Args)]
struct QuoteArgs {
    /// Sats to pay over Lightning.
    #[arg(long, conflicts_with = "usdc")]
    sats: Option<u64>,
    /// Exact USDC to receive on Base, e.g. 25 or 12.50.
    #[arg(long, conflicts_with = "sats")]
    usdc: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.global.verbose);
    if let Command::Quote(args) = &cli.command {
        return quote(&cli.global, args).await;
    }
    let client = build_client(&cli.global)?;

    match cli.command {
        Command::Quote(_) => unreachable!("handled above"),
        Command::Swap(args) => run_swap(&client, &cli.global, args).await,
        Command::Claim { swap_id } => {
            let stored = load_swap(&client, &swap_id)?;
            let response = lightning_to_evm(&stored)?;
            println!(
                "swap {}  status: {}",
                response.id,
                response.status.as_wire_str()
            );
            wait_until_claimable(&client, &swap_id, None).await?;
            claim(&client, &cli.global, &swap_id).await
        }
        Command::Status { swap_id } => {
            let details = client.get_swap_details(&swap_id).await?;
            println!("{}", serde_json::to_string_pretty(&details)?);
            Ok(())
        }
        Command::Doctor => doctor(&client, &cli.global).await,
    }
}

/// Exercise everything a claim depends on, without creating a swap.
async fn doctor(client: &Client, global: &Global) -> Result<()> {
    println!(
        "api:               {} ({})",
        global.api,
        client.health().await?
    );
    let (aa, config, rpc) = aa_setup(client, global).await?;
    println!("entry point:       {}", config.entry_point);
    println!("delegation target: {}", config.delegation_target);
    println!("bundler:           {}", host(&aa.bundler.url));
    println!(
        "paymaster:         {} (policy {})",
        aa.paymaster
            .as_ref()
            .map(|p| host(&p.url))
            .unwrap_or_default(),
        aa.paymaster
            .as_ref()
            .and_then(|p| p.policy_id.as_deref())
            .unwrap_or("none")
    );
    let mnemonic = client.mnemonic().context("client has no mnemonic")?;
    let key = Signer::from_mnemonic(mnemonic)?.derive_wallet_evm_key()?;
    let wallet = RpcWallet::connect(&key, ARBITRUM, rpc.clone()).await?;
    println!("arbitrum rpc:      {} (chain {})", host(&rpc), ARBITRUM);
    println!("wallet evm key:    {}", key.address);
    println!("chain time:        {}", wallet.block_timestamp().await?);
    println!("ok");
    Ok(())
}

fn host(url: &Url) -> String {
    format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or("<no host>")
    )
}

/// Bundler and paymaster come from Satora; the node RPC is the caller's
/// (defaulting to the bundler endpoint, which serves `eth_*` as well).
async fn aa_setup(
    client: &Client,
    global: &Global,
) -> Result<(AaConfigResponse, UserOpConfig, Url)> {
    let aa = fetch_aa_config(client).await?;
    let rpc = global.rpc.clone().unwrap_or_else(|| aa.bundler.url.clone());
    let config = UserOpConfig {
        aa: AaConfig {
            bundler_url: aa.bundler.url.clone(),
            node_rpc_url: rpc.clone(),
            paymaster: aa.paymaster.as_ref().map(|pm| PaymasterConfig {
                url: pm.url.clone(),
                context: match &pm.policy_id {
                    Some(id) => serde_json::json!({ "policyId": id }),
                    None => serde_json::Value::Null,
                },
            }),
            bundler_casing: aa.bundler.casing,
            gas_overrides: None,
        },
        entry_point: aa.entry_point,
        delegation_target: aa.delegation_target,
    };
    if config.aa.paymaster.is_none() {
        bail!("Satora returned no paymaster for Arbitrum; a sponsored claim needs one");
    }
    Ok((aa, config, rpc))
}

/// A server quote for Lightning → USDC on Base. The SDK quotes a bridge-only
/// target on the Arbitrum hub and names Base in `bridge_target_chain`, exactly
/// as `swap` creates it. The referral code (if any) sets the partner rate.
async fn quote(global: &Global, args: &QuoteArgs) -> Result<()> {
    let amount = match (args.sats, args.usdc.as_deref()) {
        (Some(sats), None) => QuoteAmount::Source(u128::from(sats)),
        (None, Some(usdc)) => QuoteAmount::Target(parse_usdc(usdc)?),
        _ => bail!("pass exactly one of --sats or --usdc"),
    };
    let mut client = Client::new(&global.api)?;
    client.set_referral_code(global.referral.clone());
    let q = client
        .get_server_quote(QuoteRequest::new(
            Chain::lightning(),
            TokenId::Btc,
            Chain::from_wire_str(&BASE.to_string()),
            TokenId::Other(USDC_BASE.to_string()),
            amount,
        ))
        .await?;
    println!("you pay:        {} sats over Lightning", q.source_amount);
    println!(
        "you receive:    {} USDC on Base",
        format_usdc(&q.net_target_amount)
    );
    println!("rate:           {} USD/BTC", q.exchange_rate);
    println!(
        "fees:           protocol {} sats ({:.2}%), network {} sats{}",
        q.protocol_fee,
        q.protocol_fee_rate * 100.0,
        q.network_fee + q.gasless_network_fee,
        q.bridge_fee
            .map(|f| format!(", CCTP bridge {} USDC", format_usdc(&f.to_string())))
            .unwrap_or_default()
    );
    println!("limits:         {}..{} sats", q.min_amount, q.max_amount);
    if let Some(code) = &global.referral {
        println!("referral:       {code}");
    }
    Ok(())
}

fn init_logging(verbose: bool) {
    let default = if verbose {
        "info,lendaswap_sdk=debug"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn data_dir(global: &Global) -> Result<PathBuf> {
    if let Some(dir) = &global.data_dir {
        return Ok(dir.clone());
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --data-dir")?;
    Ok(PathBuf::from(home).join(".satora-ln2base"))
}

fn build_client(global: &Global) -> Result<Client> {
    let dir = data_dir(global)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let wallet_db = dir.join("wallet.sqlite");
    let swaps_db = dir.join("swaps.sqlite");
    let fresh_wallet = global.mnemonic.is_none() && !wallet_db.exists();

    let mut builder = Client::builder()
        .base_url(global.api.clone())
        .storage_path(&swaps_db)
        .wallet_storage_path(&wallet_db);
    if let Some(mnemonic) = &global.mnemonic {
        builder = builder.mnemonic(mnemonic.clone());
    }
    if let Some(code) = &global.referral {
        builder = builder.referral_code(code.clone());
    }
    let client = builder.build()?;

    if fresh_wallet {
        eprintln!(
            "Generated a new wallet in {}. Back up this mnemonic; it controls your claims:\n  {}\n",
            wallet_db.display(),
            client.mnemonic().unwrap_or("<unavailable>")
        );
    }
    Ok(client)
}

async fn run_swap(client: &Client, global: &Global, args: SwapArgs) -> Result<()> {
    let to: Address = args
        .to
        .parse()
        .map_err(|e| anyhow!("--to must be an EVM address: {e}"))?;
    let amount = match (args.sats, args.usdc.as_deref()) {
        (Some(sats), None) => QuoteAmount::Source(u128::from(sats)),
        (None, Some(usdc)) => QuoteAmount::Target(parse_usdc(usdc)?),
        _ => bail!("pass exactly one of --sats or --usdc"),
    };

    let creation = SwapCreation::LightningToEvm {
        // The DEX leg runs on the Arbitrum hub; `bridge` below routes the USDC on to Base.
        target: EvmAsset {
            chain_id: ARBITRUM,
            token: TokenId::UsdcArbitrum,
        },
        amount,
        target_address: to.to_string(),
    };
    let options = SwapOptions {
        gasless: Some(true),
        invoice_description: args.memo,
        bridge: Some(BridgeRoute {
            chain: BRIDGE_CHAIN_BASE.to_string(),
            token_address: USDC_BASE.to_string(),
            recipient: None,
            recipient_wallet: None,
            recipient_setup: false,
        }),
        ..Default::default()
    };

    let swap = client.create_swap_request(creation, options).await?;
    let stored = load_swap(client, &swap.id)?;
    let response = lightning_to_evm(&stored)?;

    println!("swap id:      {}", response.id);
    println!(
        "you pay:      {} sats over Lightning",
        response.source_amount
    );
    println!(
        "you receive:  {} USDC on Base at {}",
        format_usdc(&response.target_amount),
        to
    );
    if let Some(expiry) = response.invoice_expires_at {
        println!("invoice expires: {} (unix {expiry})", human_in(expiry));
    }
    println!("\nPay this invoice:\n\n{}\n", response.bolt11_invoice);

    if args.no_claim {
        println!(
            "Created only. Claim later with: satora-ln2base claim {}",
            response.id
        );
        return Ok(());
    }

    wait_until_claimable(client, &swap.id, response.invoice_expires_at).await?;
    claim(client, global, &swap.id).await
}

/// Poll until the server has funded the Arbitrum HTLC (claimable) or the swap
/// reached a terminal state. Prints each status change.
async fn wait_until_claimable(
    client: &Client,
    swap_id: &str,
    invoice_expires_at: Option<i64>,
) -> Result<()> {
    let deadline = invoice_expires_at
        .map(|t| Duration::from_secs(t.saturating_sub(now_unix()).max(0) as u64 + 15 * 60))
        .unwrap_or(Duration::from_secs(2 * 60 * 60));
    let started = Instant::now();
    let mut last: Option<SwapStatus> = None;
    println!("Waiting for payment and server funding (polling every 3s)...");
    loop {
        let status = match client.get_swap_details(swap_id).await {
            Ok(details) => details.status().clone(),
            Err(e) => {
                eprintln!("status poll failed: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        if last.as_ref() != Some(&status) {
            println!(
                "[{:>5}s] status: {}",
                started.elapsed().as_secs(),
                status.as_wire_str()
            );
            last = Some(status.clone());
        }
        match status {
            SwapStatus::ServerFunded | SwapStatus::ClientRedeeming | SwapStatus::ClientRedeemed => {
                return Ok(());
            }
            SwapStatus::ServerRedeemed => {
                println!("Swap already settled.");
                return Ok(());
            }
            SwapStatus::Expired
            | SwapStatus::ServerWontFund
            | SwapStatus::ClientFundedTooLate
            | SwapStatus::ClientFundedServerRefunded
            | SwapStatus::ClientInvalidFunded
            | SwapStatus::ClientRefunded
            | SwapStatus::ClientRefundedServerFunded
            | SwapStatus::ClientRefundedServerRefunded => {
                bail!(
                    "swap {swap_id} ended as {}; the held invoice unwinds on its own",
                    status.as_wire_str()
                );
            }
            _ => {}
        }
        if started.elapsed() > deadline {
            bail!("gave up waiting for swap {swap_id} after {deadline:?}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Claim the Arbitrum HTLC through a sponsored UserOperation whose calls swap
/// the locked BTC token to USDC and hand it to Circle's CCTP adapter for Base.
async fn claim(client: &Client, global: &Global, swap_id: &str) -> Result<()> {
    let stored = load_swap(client, swap_id)?;
    let response = lightning_to_evm(&stored)?;
    if response.evm_chain_id != ARBITRUM as i64 {
        bail!(
            "expected an Arbitrum-hub swap, got chain {}",
            response.evm_chain_id
        );
    }
    if response.bridge_target_chain.as_deref() != Some(BRIDGE_CHAIN_BASE) {
        bail!(
            "swap {swap_id} is not routed to Base (bridge_target_chain = {:?})",
            response.bridge_target_chain
        );
    }

    let (_aa, config, rpc) = aa_setup(client, global).await?;

    // The claim signer is the swap's own key (its address is `client_evm_address`).
    let mnemonic = client.mnemonic().context("client has no mnemonic")?;
    let key = Signer::from_mnemonic(mnemonic)?
        .evm_key_for_address(stored.key_index, &response.client_evm_address)?;
    let wallet = RpcWallet::connect(&key, ARBITRUM, rpc).await?;

    let destination: Address = response
        .target_evm_address
        .as_deref()
        .context("swap has no target address")?
        .parse()?;
    let options = EvmClaimOptions {
        htlc: response.evm_htlc_address.parse()?,
        coordinator: response.evm_coordinator_address.parse()?,
        destination,
        // USDC on Arbitrum is swept; the CCTP adapter drains it in the same call,
        // so the coordinator's final sweep floor is zero. The DEX calls carry
        // their own minimum output.
        sweep_token: TokenId::UsdcArbitrum.as_wire_str().parse()?,
        min_amount_out: U256::ZERO,
        minimum_remaining_seconds: 120,
        timeout: Duration::from_secs(240),
    };

    // Resume a claim this process (or a previous one) already submitted.
    if let Some(tx) = &stored.metadata.evm_claim_txid {
        println!("Claim already submitted on Arbitrum: {tx}");
        return finish(client, swap_id).await;
    }
    if stored.metadata.evm_claim_userop_hash.is_some() {
        println!("Resuming a pending sponsored claim...");
        match client
            .resume_evm_claim_userop(swap_id, &wallet, &config)
            .await?
        {
            Some(hash) => println!("Claim mined on Arbitrum: {hash}"),
            None => {
                bail!("the UserOperation is still pending; run `claim {swap_id}` again shortly")
            }
        }
        return finish(client, swap_id).await;
    }

    let calls = client
        .get_dex_calldata_fund(DexCalldataRequest {
            from: DexToken::Evm {
                chain_id: ARBITRUM,
                address: response.wbtc_address.clone(),
            },
            // Naming the Base USDC token makes the daemon append the CCTP leg.
            to: DexToken::Evm {
                chain_id: BASE,
                address: USDC_BASE.to_string(),
            },
            amount: DexAmount::ExactIn(response.evm_expected_sats.clone()),
            sender: response.evm_coordinator_address.clone(),
            slippage_bps: DEX_SLIPPAGE_BPS,
            bridge_recipient: Some(BridgeRecipient::Evm {
                address: destination.to_string(),
            }),
            // Lets the server deliver the bridged payout itself, without
            // Circle's forwarding fee.
            swap_id: Some(swap_id.to_string()),
        })
        .await
        .context("fetch DEX + CCTP claim calls")?;
    let DexCallsPayload::Evm { calls } = calls.payload;

    println!(
        "Claiming via sponsored UserOperation ({} coordinator calls)...",
        calls.len()
    );
    let mut attempt = 0;
    let receipt = loop {
        attempt += 1;
        match client
            .claim_evm_userop(swap_id, &wallet, options.clone(), calls.clone(), &config)
            .await
        {
            Ok(receipt) => break receipt,
            Err(e) => {
                // Anything that may have left the process must be resumed, not retried.
                let now = load_swap(client, swap_id)?;
                if now.metadata.evm_claim_userop_hash.is_some()
                    || now.metadata.evm_claim_txid.is_some()
                {
                    return Err(anyhow!(
                        "claim submission is in flight ({e}); run `claim {swap_id}` to resume"
                    ));
                }
                if attempt >= 4 {
                    return Err(anyhow!("claim failed after {attempt} attempts: {e}"));
                }
                eprintln!("claim attempt {attempt} failed: {e}; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    println!("UserOperation: {}", receipt.user_op_hash);
    match receipt.transaction_hash {
        Some(hash) => println!("Claim mined on Arbitrum: {hash}"),
        None => println!("Bundler accepted the operation; inclusion not yet observed."),
    }
    finish(client, swap_id).await
}

/// Wait for the server to settle the Lightning side, then report.
async fn finish(client: &Client, swap_id: &str) -> Result<()> {
    println!("Waiting for the server to settle the invoice...");
    let status = client
        .wait_for_swap_status(
            swap_id,
            &[SwapStatus::ClientRedeemed, SwapStatus::ServerRedeemed],
            Duration::from_secs(10 * 60),
        )
        .await?;
    println!("Final status: {}", status.as_wire_str());
    println!("USDC is being minted on Base through CCTP; it usually arrives within a few minutes.");
    Ok(())
}

// --- helpers -----------------------------------------------------------------

fn load_swap(client: &Client, swap_id: &str) -> Result<StoredSwap> {
    client
        .get_stored_swap(swap_id)?
        .with_context(|| format!("swap {swap_id} is not in local storage"))
}

fn lightning_to_evm(stored: &StoredSwap) -> Result<&LightningToEvmSwapResponse> {
    match &stored.response {
        GetSwapResponse::LightningToEvm(r) => Ok(r),
        other => bail!("swap {} is not Lightning→EVM", other.id()),
    }
}

/// `GET /aa/config?chain=42161`, including the paymaster policy ID the SDK's
/// own `RemoteAaConfig` does not surface.
#[derive(Serialize)]
struct AaConfigQuery {
    chain: Chain,
}
impl Endpoint for AaConfigQuery {
    type Response = AaConfigResponse;
    const METHOD: reqwest::Method = reqwest::Method::GET;
    const PATH: &'static str = "aa/config";
    const PAYLOAD: PayloadKind = PayloadKind::Query;
}
#[derive(Deserialize)]
struct AaConfigResponse {
    entry_point: Address,
    delegation_target: Address,
    bundler: BundlerInfo,
    paymaster: Option<PaymasterInfo>,
}
#[derive(Deserialize)]
struct BundlerInfo {
    url: Url,
    casing: BundlerCasing,
}
#[derive(Deserialize)]
struct PaymasterInfo {
    url: Url,
    policy_id: Option<String>,
}
async fn fetch_aa_config(client: &Client) -> Result<AaConfigResponse> {
    client
        .send(AaConfigQuery {
            chain: Chain::Known(KnownChain::Arbitrum),
        })
        .await
        .context("fetch Arbitrum AA config from Satora")
}

fn parse_usdc(text: &str) -> Result<u128> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if frac.len() > 6
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("--usdc must be a decimal with at most 6 fraction digits, e.g. 12.50");
    }
    let whole: u128 = whole.parse()?;
    let frac: u128 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse()?
    };
    whole
        .checked_mul(1_000_000)
        .and_then(|w| w.checked_add(frac))
        .filter(|v| *v > 0)
        .context("--usdc must be positive")
}

fn format_usdc(micro: &str) -> String {
    match micro.parse::<u128>() {
        Ok(v) => format!("{}.{:06}", v / 1_000_000, v % 1_000_000),
        Err(_) => micro.to_string(),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_in(unix: i64) -> String {
    let delta = unix - now_unix();
    if delta <= 0 {
        return "already expired".into();
    }
    format!("in {}m{:02}s", delta / 60, delta % 60)
}
