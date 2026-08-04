//! surf-bridge — the BLE half of the BLUETOOTH BROWSER stack.
//!
//! Connects a Passport Prime (Nordic UART Service) directly to the
//! surf-relay on the box: no phone, no Envoy. The Passport pairs with this
//! bridge exactly as it would pair with a companion app; web-0 envelopes ride
//! Quantum Link over BLE and are forwarded to the relay (TCP 8787).
//!
//! ```
//! Dojo Signer / Browser ──BLE──> surf-bridge (this) ──TCP──> surf-relay ──> bitcoind
//! ```
//!
//! Usage:
//!   surf-bridge --relay 127.0.0.1:8787                # scan + connect + bridge
//!   surf-bridge --mac <device-mac> --relay 127.0.0.1:8787
//!   surf-bridge --sim --relay 127.0.0.1:8787          # TCP self-test, no BLE

mod ble;
mod identity;
mod ql;
mod relay;
mod router;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use btleplug::api::{Central, Manager as _};
use clap::Parser;
use foundation_api::dcbor::CBOREncodable;
use foundation_api::message::{EnvoyMessage, QuantumLinkMessage};
use foundation_api::quantum_link::{QuantumLink, QuantumLinkIdentity};
use futures::StreamExt;

use crate::identity::BridgeState;

#[derive(Parser, Debug)]
#[command(name = "surf-bridge", about = "Passport Prime ↔ surf-relay BLE bridge")]
struct Args {
    /// surf-relay address (host:port)
    #[arg(long, default_value = "127.0.0.1:8787")]
    relay: String,

    /// Device MAC address; omit to scan for any Passport
    #[arg(long)]
    mac: Option<String>,

    /// Seconds to scan for the device
    #[arg(long, default_value_t = 10)]
    scan_seconds: u64,

    /// Directory for persisted bridge identity + pairing state
    #[arg(long, default_value = "~/.surf-bridge")]
    state_dir: String,

    /// Relay timeout in seconds
    #[arg(long, default_value_t = 30)]
    timeout: u64,

    /// TCP self-test mode: exercise the QL envelope pipeline without BLE
    #[arg(long)]
    sim: bool,

    /// Pair with a device whose XID document we already know (hex of the
    /// device's pairing QR payload), then run the bridge. Persists the
    /// device XID in --state-dir so later runs skip pairing.
    #[arg(long)]
    pair_xid: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    let args = Args::parse();
    let state_dir = expand_tilde(&args.state_dir);

    if args.sim {
        return run_sim(&args).await;
    }

    // ---- Load or create our companion identity ----
    let mut state = BridgeState::load_or_create(&state_dir)
        .context("load bridge identity")?;
    log::info!(
        "bridge XID: {}",
        hex::encode(state.identity.xid_document.to_cbor_data())
    );

    // ---- Optional first-time pairing: store the device XID from its QR ----
    if let Some(xid_hex) = &args.pair_xid {
        let raw = hex::decode(xid_hex).context("--pair-xid must be hex")?;
        let cbor = foundation_api::dcbor::CBOR::try_from_data(&raw)
            .context("device XID is not valid CBOR")?;
        let device_xid = foundation_api::bc_xid::XIDDocument::try_from(cbor)
            .context("device XID is not a valid XID document")?;
        state.device_xid = Some(device_xid);
        state.save(&state_dir)?;
        log::info!("saved paired device XID");
    }

    // ---- BLE: find + connect to the Passport ----
    let manager = btleplug::platform::Manager::new()
        .await
        .context("btleplug manager")?;
    let adapters = manager.adapters().await.context("list adapters")?;
    let adapter = adapters
        .first()
        .ok_or_else(|| anyhow!("no bluetooth adapter found on this machine"))?;
    log::info!("using adapter: {}", adapter.adapter_info().await?);

    let peripheral =
        ble::find_passport(adapter, args.mac.as_deref(), args.scan_seconds).await?;
    log::info!("connecting…");
    let session = ble::NusSession::connect(&peripheral).await?;

    // ---- If we know the device XID, send the initial PairingRequest ----
    // so the device accepts us as its companion (it auto-replies
    // PairingResponse; after that web-0 traffic flows). Safe to re-send;
    // the device ignores pairing requests when already paired.
    if let Some(device_xid) = &state.device_xid {
        let router = router::BridgeRouter::new(
            state.identity.clone(),
            &args.relay,
            Duration::from_secs(args.timeout),
        );
        log::info!("sending PairingRequest to known device…");
        match router.build_pairing_request(device_xid, "surf-bridge") {
            Ok(frames) => {
                for f in frames {
                    session.write(&f).await?;
                }
            }
            Err(e) => log::warn!("pairing request failed: {e:#}"),
        }
        drop(router);
    }

    // ---- Run the bridge loop ----
    let mut router = router::BridgeRouter::new(
        state.identity,
        &args.relay,
        Duration::from_secs(args.timeout),
    );
    let mut notifications = session.notifications().await?;

    log::info!(
        "🟢 surf-bridge live — device ↔ relay {} (Ctrl-C to stop)",
        args.relay
    );
    while let Some(frame) = notifications.next().await {
        match router.on_frame(&frame) {
            Ok(replies) => {
                for reply in replies {
                    session.write(&reply).await?;
                }
            }
            Err(e) => log::warn!("frame error: {e:#}"),
        }
    }
    Err(anyhow!("device disconnected"))
}

// ---------------------------------------------------------------------------
// TCP self-test: prove the whole QL pipeline (seal → chunk → dechunk →
// unseal → route → relay → seal back) without needing BLE hardware.
// ---------------------------------------------------------------------------

async fn run_sim(args: &Args) -> Result<()> {
    use foundation_api::bitcoin::AccountUpdate;
    use foundation_api::message::PassportMessage;

    let state = BridgeState::load_or_create(&expand_tilde(&args.state_dir))?;
    let bridge = state.identity;
    let device = QuantumLinkIdentity::generate();

    log::info!("SIM: simulating a device sending a fetch envelope over QL…");

    // Device builds the same AccountUpdate the KeyOS browser app sends.
    let device_msg = PassportMessage {
        message: QuantumLinkMessage::AccountUpdate(AccountUpdate {
            account_id: "web-0".to_string(),
            update: br#"{"type":"fetch","id":"1","url":"https://example.com/"}"#.to_vec(),
        }),
        status: foundation_api::status::DeviceStatus {
            version: "0.1.0".into(),
            battery_level: 88,
        },
        protocol_version: Some(1),
    };
    // Device seals TO the bridge's XID (mirrors the real pairing trust).
    let sealed = foundation_api::quantum_link::QuantumLink::seal(
        device_msg,
        (device.private_keys.as_ref().unwrap(), &device.xid_document),
        &bridge.xid_document,
    );
    let cbor = sealed.to_cbor_data();
    let frames: Vec<Vec<u8>> = btp::chunk(&cbor).map(|c| c.to_vec()).collect();

    // Feed through the router (which talks to the real surf-relay over TCP).
    let mut router = router::BridgeRouter::new(
        bridge.clone(),
        &args.relay,
        Duration::from_secs(args.timeout),
    );
    let mut replies: Vec<Vec<u8>> = Vec::new();
    for frame in &frames {
        replies.extend(router.on_frame(frame)?);
    }

    // Bridge's reply must come back as a sealed EnvoyMessage AccountUpdate
    // the *device* can unseal with its own keys.
    let mut arid = foundation_api::quantum_link::ARIDCache::new();
    let mut got_page = false;
    // The sealed reply is BTP-chunked too — feed ALL frames through ONE
    // shared dechunker before unsealing.
    let mut reply_dechunker = crate::ql::Dechunker::default();
    let mut complete = None;
    for reply in &replies {
        if let Some(c) = reply_dechunker.push(reply)? {
            complete = Some(c);
        }
    }
    if let Some(cbor) = complete {
        let cbor_obj =
            foundation_api::dcbor::CBOR::try_from_data(&cbor)?;
        let envelope =
            foundation_api::bc_envelope::Envelope::try_from_cbor(cbor_obj)?;
        let (msg, _sender) = EnvoyMessage::unseal_envoy_message_with_replay_check(
            &envelope,
            device.private_keys.as_ref().unwrap(),
            &mut arid,
        )
        .map_err(|e| anyhow!("device failed to unseal bridge reply: {e:?}"))?;
        if let QuantumLinkMessage::AccountUpdate(upd) = msg.message {
            if upd.account_id == "web-0" {
                let page = String::from_utf8_lossy(&upd.update);
                log::info!("SIM: device received reply: {page}");
                assert!(
                    page.contains("Example Domain") && page.contains("\"status\": 200"),
                    "expected a page envelope, got: {page}"
                );
                got_page = true;
            }
        }
    }
    assert!(got_page, "device never received the page envelope");
    log::info!("✅ SIM OK — full QL round-trip through the live relay works");
    Ok(())
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}
