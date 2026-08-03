# 🌐 BLUETOOTH BROWSER — web on a BLE-only hardware wallet

A **system browser for Passport Prime (KeyOS)**. The Passport has no Wi-Fi and
no cellular — only Bluetooth LE — so it can never reach the internet alone.
BLUETOOTH BROWSER turns that constraint into a feature:

```
┌────────────────────────────  Passport Prime  ────────────────────────────┐
│  BLUETOOTH BROWSER (this app)                                            │
│   • address bar · page blocks · links · back/forward · history           │
│   • renders ONLY safe structured blocks — never HTML/JS                  │
│   • other KeyOS apps ask it to open URLs (shared web surface)            │
└───────────────▲───────────────────────────────────────────────────────────┘
                │ Quantum Link — JSON envelopes, channel "web-0" (BLE / sim)
┌───────────────┴───────────────────────────────────────────────────────────┐
│  surf-relay  (your box / Mac / any gateway with internet)                 │
│   • receives fetch requests · downloads pages (Tor or clearnet)           │
│   • HTML → clean blocks: text, headings, links, quotes, code              │
│   • scripts, styles, tracking and unsafe links stripped                   │
└────────────────────────────────────────────────────────────────────────────┘
```

**The security model is untouched:** the signing device stays a signing
device. The browser touches **no seed, no wallet, no key material** — it only
renders text the relay prepared. A hostile page cannot execute anything on the
device, and the relay caps page size and block count so a hostile page cannot
exhaust device memory.

## ✨ Features

- **System browser** — one shared web surface. Any KeyOS app (RoboSats,
  Dojo Signer, a future exchange) asks the browser to open a URL via the
  `open-url` channel; the browser fetches and renders it.
- **Address bar + GO** with on-device keyboard input.
- **Back / forward / home** with a per-session history (re-fetches cached
  pages instantly).
- **Page blocks**: headings, paragraphs, tappable links, quotes, code,
  separators — a readable web on a 480×800 screen.
- **Relay status**: pairing + relay-online indicators on the start page.
- **Demo mode**: without a relay (e.g., the simulator), the app still boots to
  a start page and can render sample pages so the UI is fully explorable.

## 🧱 Repo layout

```
├── gui-app-browser/        # the KeyOS app (slint UI + QL wiring)
│   ├── src/main.rs         # fetch/page state machine, history, QL channel
│   └── ui/                 # app.slint + home/browse pages + callbacks
├── relay/surf_relay.py     # the internet gateway (python3, stdlib only)
└── docs/PROTOCOL.md        # the companion ↔ device wire spec
```

## 🚀 Run it

**Relay (any machine with python3):**

```bash
python3 relay/surf_relay.py --port 8787                 # clearnet
python3 relay/surf_relay.py --socks 127.0.0.1:9050      # through Tor
python3 relay/surf_relay.py --self-test https://example.com
```

**App:** register `gui-app-browser` in the KeyOS workspace (workspace member +
xtask DEFAULT lists), then `cargo xtask run --hosted` and open
**BLUETOOTH BROWSER** on the home screen.

## 🔐 Security

| Property | Guarantee |
|---|---|
| No network on device | QL envelopes only — the device never opens a socket |
| No script on device | Relay strips JS/CSS; device renders text blocks |
| Safe links only | Absolute http(s) only; javascript:/data: dropped |
| Size caps | 512 KB pages, bounded block count |
| No keys | The browser never touches the seed or wallet |

## 📜 License

GPL-3.0-or-later — matching the KeyOS ecosystem.
