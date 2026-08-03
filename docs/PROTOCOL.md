# BLUETOOTH BROWSER × KeyOS — Companion/Relay ↔ Device Protocol

Wire specification for the **BLUETOOTH BROWSER** app on Passport Prime (KeyOS)
and its internet gateway (the *surf-relay*). The device is BLE-only and has no
network stack: every web request rides over Quantum Link as a small JSON
envelope and the relay does the fetching + rendering. **No HTML or JavaScript
ever reaches the device — only safe structured blocks.**

## 1. Transport

- **Medium:** Quantum Link (BLE on hardware; simulated in the hosted sim),
  archive messages.
- **Channel/account id:** `web-0` (the `account_id` field on every message).
- **Payload:** JSON, UTF-8, one envelope per message, lowercase-snake_case.
- The relay's TCP mode (one JSON envelope per line, port 8787) speaks the same
  envelopes — it is the transport a future BLE bridge fronts.

## 2. Envelopes

### 2.1 `fetch` (device → relay)

```json
{ "type": "fetch", "id": "7", "url": "https://example.com/" }
```

- `id` — opaque, echoed back in the `page` reply so the device can match
  responses to in-flight requests.
- `url` — must be `http(s)://`; the relay rejects anything else.

### 2.2 `page` (relay → device)

```json
{
  "type": "page",
  "id": "7",
  "status": 200,
  "title": "Example Domain",
  "url": "https://example.com/",
  "blocks": [
    { "kind": "heading", "text": "Example Domain", "level": 1 },
    { "kind": "text", "text": "This domain is for use in illustrative examples." },
    { "kind": "link", "text": "More information...", "href": "https://www.iana.org/domains/example" },
    { "kind": "quote", "text": "quoted line" },
    { "kind": "code", "text": "pre-formatted text" },
    { "kind": "separator", "text": "──────" }
  ],
  "error": null
}
```

- `status` — HTTP status, or `0` if the fetch failed (see `error`).
- `blocks` — the page body. `kind` is one of:
  `heading` (level 1-6) · `text` · `link` (with `href`, already absolute and
  http(s)-only) · `quote` · `code` · `separator`.
- The relay hard-limits the page (512 KB default) and the block count; lists
  are emitted as individual `text`/`link` blocks; scripts, styles and images
  are stripped; `javascript:`/`data:`/`mailto:` links are dropped.

### 2.3 `open-url` (relay → device, push)

```json
{ "type": "open-url", "url": "https://robosats.com/" }
```

The gateway (or another app's companion flow) asks the browser to open a page.
The browser fetches it through the same channel and switches to its browse
view. This is the hook other KeyOS apps use to surface web content in the
shared browser.

### 2.4 `ready` (device → relay)

```json
{ "type": "ready", "version": "1.0" }
```

Announced on startup so the gateway knows the browser is present.

### 2.5 `ping` / `pong`

Heartbeat used by the TCP transport and future BLE bridge.

### 2.6 `broadcast` (device → relay) and `broadcast-result` (relay → device)

The relay is also a **transaction broadcast gateway**: a signed raw
transaction is submitted to a Bitcoin node and the txid is returned. This is
how Dojo Signer (or any KeyOS app) gets a signed spend on-chain without the
device ever opening a socket and without trusting a third-party API.

```json
{ "type": "broadcast", "id": "12", "txhex": "02000000000101..." }
{ "type": "broadcast", "id": "13", "psbt": "cHNidP8BAH0CAAAA..." }
```

- `txhex` — a fully-signed, finalized raw transaction (hex).
- `psbt` — *or* a signed PSBT (base64). The relay asks its bitcoind to
  finalize it (`finalizepsbt`) and then broadcasts the resulting hex. This is
  the natural payload for signing devices, which produce PSBTs.
- The relay sanity-checks the hex shape, then submits via
  `sendrawtransaction` to the configured bitcoind (default `127.0.0.1:8332`,
  cookie auth — see `--rpc-url` / `--rpc-cookie`). The node performs the real
  validation.

Reply:

```json
{ "type": "broadcast-result", "id": "12", "txid": "a1b2...", "error": null }
```

- `txid` — the node-confirmed txid on success, else `null`.
- `error` — `null` on success, or a human-readable reason (bad hex, node
  unreachable, `sendrawtransaction` rejection) for the device to display.

### 2.7 `ready` → `broadcast` flow

On hardware the relay runs on the gateway the companion fronts, so the same
envelope protocol works end-to-end: sign on the device, transmit over QL to
the companion, companion hands the envelope to the relay, relay submits to
your node, txid rides back to the device.

## 3. Security properties

| Property | Guarantee |
|---|---|
| **No network on device** | The Passport never opens a socket; QL envelopes only. |
| **No script on device** | The relay strips JS/CSS; the device renders text blocks. |
| **Safe links only** | Only absolute `http(s)` links are forwarded. |
| **Size caps** | Pages are truncated and block-count-limited; a hostile page can't exhaust device memory. |
| **Your gateway, your rules** | Run the relay with `--socks` (Tor) for full privacy, or clearnet. |
| **No keys involved** | The browser touches no seed, no wallet, no signing. |
| **Broadcast privacy** | `broadcast` submits to **your node** via cookie auth — no public API, no third party |

## 4. Relay (gateway) deployment

```bash
# clearnet gateway (any box/mac with python3)
python3 surf_relay.py --port 8787

# privacy gateway — everything through Tor (needs a local Tor SOCKS on :9050)
python3 surf_relay.py --socks 127.0.0.1:9050

# one-shot check
python3 surf_relay.py --self-test https://example.com
```

Future transports front the same envelope protocol: Web Bluetooth (browser
companion), a phone daemon, or the KeyOS hosted sim bridge.
