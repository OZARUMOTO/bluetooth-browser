# 📡 surf-bridge — Passport Prime ↔ surf-relay, no phone required

The **BLE half** of the BLUETOOTH BROWSER stack. `surf-bridge` is a small Rust
daemon that runs on your box (the same machine as the surf-relay) and lets the
Passport Prime talk to it **directly over Bluetooth** — no phone, no Envoy, no
third party.

```
┌────────────────────────  Passport Prime  ────────────────────────┐
│  Dojo Signer / BLUETOOTH BROWSER (KeyOS app)                     │
│  "cold signs, bridge transmits"                                  │
└──────────────▲───────────────────────────────────────────────────┘
               │ BLE — Nordic UART Service (NUS)
               │ Quantum Link: sealed GSTP envelopes (Kyber), BTP-chunked
┌──────────────▼───────────────────────────────────────────────────┐
│  surf-bridge (this crate, on your box)                           │
│   • pairs with the Passport exactly like a companion app         │
│   • unseals web-0 envelopes with your own XID identity           │
│   • forwards them to the surf-relay over TCP                     │
└──────────────▲───────────────────────────────────────────────────┘
               │ TCP 8787 — one JSON envelope per line
┌──────────────▼───────────────────────────────────────────────────┐
│  surf-relay (relay/surf_relay.py)                                │
│   • fetch pages → safe blocks · broadcast txs → YOUR bitcoind    │
└──────────────────────────────────────────────────────────────────┘
```

The Passport cannot be made to reach the internet on its own — it is BLE-only
hardware by design. But the "companion" does not have to be a phone app:
**the box itself has a Bluetooth radio**, and this daemon is the bridge that
replaces Envoy.

## Why this is secure

The QL wire protocol is the same one Envoy uses — sealed bc-envelopes with
post-quantum (Kyber) encryption, replay protection (ARID), and XID identity
pinning. The bridge reuses Foundation's own crypto crates (`foundation-api`,
`btp` from the OZARUMOTO fork, tag `5.6.2-dojo-vault`) — it does **not**
reimplement any crypto, and it never sees the device's seed or keys.

## Build & run

Requires a Linux box with BlueZ and a Bluetooth adapter:

```bash
sudo apt-get install -y bluez libdbus-1-dev   # radio + dbus dev headers
cargo build --release

# First run: pair with the Passport (see "Pairing" below), then:
./target/release/surf-bridge --relay 127.0.0.1:8787
```

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--relay` | `127.0.0.1:8787` | surf-relay address |
| `--mac` | scan | specific Passport MAC (e.g. `A1:B2:C3:D4:E5:F6`) |
| `--scan-seconds` | `10` | how long to scan for the device |
| `--state-dir` | `~/.surf-bridge` | persisted bridge identity + pairing |
| `--timeout` | `30` | relay timeout (s) |
| `--pair-xid` | — | hex of the device's XID document (from its pairing QR) — sends the sealed `PairingRequest` on connect and stores the device XID |
| `--sim` | — | TCP self-test of the full QL pipeline (no BLE needed) |

## Pairing (the honest wrinkle)

QL pairing is bootstrapped by exchanging XID documents: the Passport shows
its XID document on screen as a QR, and the companion reads it with a camera.
The box has no camera, so you capture the device's XID once and hand it to
the bridge. The bridge then sends a sealed `PairingRequest` on connect — the
device's QL server auto-accepts it (exactly like it accepts Envoy) and replies
`PairingResponse`, after which web-0 traffic flows. Options for the one-time
XID capture, easiest first:

1. **Phone QR scan**: open any QR reader (or Envoy) on your phone, point it
   at the Passport's pairing QR, and copy the decoded XID document hex.
2. **USB webcam** (recommended long-term): point a cheap webcam at the device
   screen; QR decoding + auto-`--pair-xid` is a small follow-up.
3. **Manual**: on-device XID export if the firmware ever exposes one.

```bash
# One time:
./target/release/surf-bridge --pair-xid <device-xid-hex> --relay 127.0.0.1:8787
# Afterwards (device XID is persisted in --state-dir):
./target/release/surf-bridge --relay 127.0.0.1:8787
```

After pairing, the device XID is stored in `--state-dir` and the bridge
reconnects autonomously — the phone is never needed again.

## `--sim` self-test

Exercises the complete QL envelope pipeline without BLE hardware: generate a
pair of identities (device + bridge), seal a web-0 `fetch` envelope the way
the real KeyOS app does, feed it through the router to a *live* surf-relay,
and verify the reply comes back as a sealed envelope the device can unseal:

```bash
./target/release/surf-bridge --sim --relay 127.0.0.1:8787
# ✅ SIM OK — full QL round-trip through the live relay works
```

Run this after starting the surf-relay to prove the plumbing end-to-end.

## Layout

- `src/ble.rs` — NUS transport (scan / connect / notify / write)
- `src/ql.rs` — seal/unseal + BTP chunking (mirrors Envoy's wrapper)
- `src/relay.rs` — surf-relay TCP client (same protocol as the sim)
- `src/router.rs` — the routing core (web-0 envelopes → relay → back)
- `src/identity.rs` — persisted bridge identity + device XID

## Tests

```bash
cargo test   # identity round-trip, QL seal→chunk→dechunk→unseal,
             # relay exchange against a canned relay, UUID cross-checks
```
