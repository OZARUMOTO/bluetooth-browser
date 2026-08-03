// SPDX-FileCopyrightText: 2026 Michael Totten <mike@ozaru.io>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Mutex;

use slint_keyos_platform::{
    app,
    async_archive,
    slint::{ComponentHandle, ModelRc, SharedString, VecModel},
    spawn_local, subscribe_archive,
};
use quantum_link::{
    messages::{SendAccountUpdate, SubscribeAccountUpdate, SubscribePairingEvent},
    PairingEvent,
};

quantum_link::use_api!();

/// The account id all BLUETOOTH BROWSER envelopes ride on. The surf-relay
/// gateway (and any future BLE bridge) must use this id too
/// (see docs/PROTOCOL.md).
const CHANNEL: &str = "web-0";

// ---------------------------------------------------------------------------
// Rendered page model
// ---------------------------------------------------------------------------

/// A safe, structured page block — matches the `PageBlock` struct in
/// browser-callbacks.slint. No HTML or JavaScript ever reaches the device;
/// only these blocks are rendered.
#[derive(Debug, Clone)]
struct Block {
    kind: String,
    text: String,
    href: String,
}

impl Block {
    fn text(kind: &str, text: &str) -> Self {
        Block { kind: kind.into(), text: text.into(), href: String::new() }
    }
    fn link(text: &str, href: &str) -> Self {
        Block { kind: "link".into(), text: text.into(), href: href.into() }
    }
}

/// Relay → device JSON envelopes (see docs/PROTOCOL.md §2).
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum RelayEnvelope {
    #[serde(rename = "page")]
    Page {
        // Echoed request id — the relay always includes it; we currently
        // render in arrival order (single in-flight request in practice).
        #[serde(default, rename = "id")]
        _id: String,
        #[serde(default)]
        status: u16,
        #[serde(default)]
        title: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        blocks: Vec<RelayBlock>,
        #[serde(default)]
        error: Option<String>,
    },
    /// The gateway (or another app's companion flow) asks us to open a page.
    #[serde(rename = "open-url")]
    OpenUrl { url: String },
}

#[derive(Debug, serde::Deserialize)]
struct RelayBlock {
    kind: String,
    text: String,
    #[serde(default)]
    href: String,
}

// ---------------------------------------------------------------------------
// History + in-flight fetch bookkeeping
// ---------------------------------------------------------------------------

/// URL history (newest last) and the current position in it.
static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
static HISTORY_INDEX: Mutex<usize> = Mutex::new(0);
/// Monotonic request id echoed in `fetch`/`page` envelopes.
static FETCH_SEQ: Mutex<u64> = Mutex::new(0);
/// True once any relay reply has been received (so we can tell "offline
/// demo" from "live relay" in the UI).
static RELAY_CONTACTED: Mutex<bool> = Mutex::new(false);

// ---------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------

fn render_blocks(ui: &AppWindow, blocks: &[Block]) {
    let model: Vec<PageBlock> = blocks
        .iter()
        .map(|b| PageBlock {
            kind: b.kind.clone().into(),
            text: b.text.clone().into(),
            href: b.href.clone().into(),
        })
        .collect();
    ui.global::<BrowserCallbacks>().set_blocks(ModelRc::new(VecModel::from(model)));
}

fn set_url(ui: &AppWindow, url: &str) {
    ui.global::<BrowserCallbacks>().set_url(url.into());
}

fn set_status(ui: &AppWindow, text: &str) {
    ui.global::<BrowserCallbacks>().set_status_line(text.into());
}

fn set_error(ui: &AppWindow, text: &str) {
    ui.global::<BrowserCallbacks>().set_error_text(text.into());
}

/// Sync the back/forward affordances from HISTORY + HISTORY_INDEX.
fn sync_nav_state(ui: &AppWindow) {
    let hist = HISTORY.lock().unwrap();
    let idx = *HISTORY_INDEX.lock().unwrap();
    let can_back = idx > 0;
    let can_forward = idx + 1 < hist.len();
    ui.global::<BrowserCallbacks>().set_can_go_back(can_back);
    ui.global::<BrowserCallbacks>().set_can_go_forward(can_forward);
}

/// Record `url` as a new history entry (truncating any forward entries).
fn push_history(ui: &AppWindow, url: &str) {
    let mut hist = HISTORY.lock().unwrap();
    let mut idx = HISTORY_INDEX.lock().unwrap();
    hist.truncate(*idx + 1);
    hist.push(url.to_string());
    *idx = hist.len() - 1;
    drop(hist);
    drop(idx);
    sync_nav_state(ui);
}

/// Fire-and-forget device → relay envelope over the web-0 channel.
fn send_envelope(envelope: serde_json::Value) {
    let message = SendAccountUpdate {
        account_id: CHANNEL.into(),
        update: serde_json::to_vec(&envelope).unwrap_or_default(),
    };
    spawn_local(async move {
        match async_archive::<quantum_link_permissions::QuantumLinkPermissions, _>(message).await {
            Ok(_) => {}
            Err(e) => log::error!("📤 QL send failed: {:?}", e),
        }
    })
    .detach();
}

/// Normalize a user-entered URL: trim, and prepend https:// to bare domains
/// so the relay accepts them.
fn normalize_url(raw: &str) -> String {
    let url = raw.trim().to_string();
    if url.is_empty() {
        return url;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("https://{}", url)
    }
}

/// Send a `fetch` envelope to the relay and render an offline demo page
/// immediately if the relay has never been contacted (simulator demo mode).
fn request_page(ui: &AppWindow, raw_url: &str, record: bool) {
    let url = normalize_url(raw_url);
    if url.is_empty() {
        return;
    }

    set_url(ui, &url);
    set_error(ui, "");
    ui.global::<BrowserCallbacks>().set_loading(true);

    if record {
        push_history(ui, &url);
    } else {
        sync_nav_state(ui);
    }

    // 1) Ask the relay over Quantum Link.
    let seq = {
        let mut s = FETCH_SEQ.lock().unwrap();
        *s += 1;
        *s
    };
    let envelope = serde_json::json!({
        "type": "fetch",
        "id": format!("{}", seq),
        "url": url,
    });
    send_envelope(envelope);
    log::info!("🌐 fetch #{}: {}", seq, url);

    // 2) Offline demo fallback (no relay ever contacted yet): show built-in
    // demo pages so the hosted simulator is fully explorable.
    if !*RELAY_CONTACTED.lock().unwrap() {
        match demo_page(&url) {
            Some((title, blocks)) => {
                ui.global::<BrowserCallbacks>().set_page_title(title.into());
                render_blocks(ui, &blocks);
                set_status(ui, "demo content — connect surf-relay for live web");
            }
            None => {
                render_blocks(ui, &[]);
                set_status(ui, "fetching…");
            }
        }
    } else {
        set_status(ui, "fetching…");
    }
    ui.global::<BrowserCallbacks>().set_loading(false);
}

// ---------------------------------------------------------------------------
// Offline demo pages (hosted simulator, no relay)
// ---------------------------------------------------------------------------

fn demo_page(url: &str) -> Option<(String, Vec<Block>)> {
    if url.starts_with("https://example.com") {
        return Some((
            "Example Domain".into(),
            vec![
                Block::text("heading", "Example Domain"),
                Block::text("text", "This domain is for use in illustrative examples in documents."),
                Block::text("text", "You may use this domain in literature without prior coordination or asking for permission."),
                Block::link("More information…", "https://www.iana.org/domains/example"),
                Block::text("separator", "──────"),
                Block::text("text", "[fetched through your surf-relay gateway — no HTML, no scripts, just blocks]"),
            ],
        ));
    }
    if url.starts_with("https://www.iana.org/domains/example") {
        return Some((
            "Example Domains | IANA".into(),
            vec![
                Block::text("heading", "Example Domains"),
                Block::text("text", "IANA manages the .example top-level domain for documentation."),
                Block::text("text", "The domain names example.com, example.net, example.org, and example.edu are reserved for use in documentation as examples."),
                Block::link("IANA home", "https://www.iana.org/"),
                Block::link("example.com", "https://example.com/"),
                Block::text("separator", "──────"),
                Block::text("text", "[demo page — rendered on-device without the internet]"),
            ],
        ));
    }
    if url.starts_with("https://bitcoin.org") {
        return Some((
            "Bitcoin.org".into(),
            vec![
                Block::text("heading", "Bitcoin"),
                Block::text("text", "Bitcoin is an innovative payment network and a new kind of money."),
                Block::text("text", "Bitcoin uses peer-to-peer technology to operate with no central authority: managing transactions and issuing money are carried out collectively by the network."),
                Block::link("Getting started", "https://bitcoin.org/en/getting-started"),
                Block::link("FAQ", "https://bitcoin.org/en/faq"),
                Block::text("separator", "──────"),
                Block::text("text", "[demo page — connect surf-relay for the live site]"),
            ],
        ));
    }
    if url.starts_with("https://github.com/OZARUMOTO/bluetooth-browser") {
        return Some((
            "OZARUMOTO/bluetooth-browser".into(),
            vec![
                Block::text("heading", "BLUETOOTH BROWSER"),
                Block::text("text", "Web over Bluetooth for Passport Prime (KeyOS). The device is BLE-only — every web request rides over Quantum Link and the surf-relay gateway does the fetching and rendering."),
                Block::text("text", "No HTML or JavaScript ever reaches the device — only safe structured blocks."),
                Block::link("docs/PROTOCOL.md", "https://github.com/OZARUMOTO/bluetooth-browser"),
                Block::link("relay/surf_relay.py", "https://github.com/OZARUMOTO/bluetooth-browser"),
                Block::text("separator", "──────"),
                Block::text("text", "[demo page — the live repo renders over the relay]"),
            ],
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

app!("BLUETOOTH BROWSER");
fn app_main(_cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("🌐 BLUETOOTH BROWSER v0.1 starting...");

    let cb = ui.global::<BrowserCallbacks>();
    cb.set_connection_status("Not connected".into());
    cb.set_relay_online(false);
    cb.set_status_line("ready — no relay contacted yet".into());

    let ui_weak = ui.as_weak();

    // ---- Navigation ----
    {
        let global = ui.global::<BrowserCallbacks>();
        global.on_goto_browse({
            let ui_weak = ui_weak.clone();
            move || {
                let ui = ui_weak.unwrap();
                ui.global::<Navigate>().invoke_browse(NavigateOptions {
                    replace: false,
                    animate: Animate::None,
                });
            }
        });
        global.on_go_home({
            let ui_weak = ui_weak.clone();
            move || {
                let ui = ui_weak.unwrap();
                ui.global::<Navigate>().invoke_home(NavigateOptions {
                    replace: false,
                    animate: Animate::None,
                });
            }
        });
    }

    // ---- Open a URL (from home quick-links, links, or submit) ----
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_open_url(move |url: SharedString| {
            let ui = ui_weak.unwrap();
            request_page(&ui, &url, true);
            // Jump straight to the browse view so the page is visible.
            ui.global::<Navigate>().invoke_browse(NavigateOptions {
                replace: false,
                animate: Animate::None,
            });
        });
    }

    // ---- Link clicked inside a page ----
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_link_clicked(move |href: SharedString| {
            let ui = ui_weak.unwrap();
            request_page(&ui, &href, true);
        });
    }

    // ---- Address bar submit ----
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_submit_url(move || {
            let ui = ui_weak.unwrap();
            let url = ui.global::<BrowserCallbacks>().get_url().to_string();
            request_page(&ui, &url, true);
        });
    }

    // ---- Back / forward / refresh ----
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_go_back(move || {
            let ui = ui_weak.unwrap();
            let mut idx = HISTORY_INDEX.lock().unwrap();
            if *idx > 0 {
                *idx -= 1;
                let url = HISTORY.lock().unwrap()[*idx].clone();
                drop(idx);
                request_page(&ui, &url, false);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_go_forward(move || {
            let ui = ui_weak.unwrap();
            let mut idx = HISTORY_INDEX.lock().unwrap();
            let len = HISTORY.lock().unwrap().len();
            if *idx + 1 < len {
                *idx += 1;
                let url = HISTORY.lock().unwrap()[*idx].clone();
                drop(idx);
                request_page(&ui, &url, false);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.global::<BrowserCallbacks>().on_refresh(move || {
            let ui = ui_weak.unwrap();
            let url = ui.global::<BrowserCallbacks>().get_url().to_string();
            request_page(&ui, &url, false);
        });
    }

    // ---- Pairing events → connection status ----
    {
        let ui_weak = ui_weak.clone();
        spawn_local(async move {
            let mut pair_events = subscribe_archive::<quantum_link_permissions::QuantumLinkPermissions, _>(
                SubscribePairingEvent,
            );
            while let Some(event) = pair_events.next().await {
                let ui = ui_weak.unwrap();
                let status = match event {
                    PairingEvent::PairingComplete { device_name, new } => {
                        format!("Paired: {} ({})", device_name, if new { "new" } else { "existing" })
                    }
                    PairingEvent::Disconnected => "Not paired".into(),
                    PairingEvent::PairingFailed => "Pairing failed".into(),
                    PairingEvent::RequestReceived => "Pairing request — approve on companion".into(),
                };
                ui.global::<BrowserCallbacks>().set_connection_status(status.into());
                log::info!(
                    "🔗 Companion pairing: {}",
                    ui.global::<BrowserCallbacks>().get_connection_status()
                );
            }
        })
        .detach();
    }

    // ---- Relay replies: `page` and `open-url` on the web-0 channel ----
    {
        let ui_weak = ui_weak.clone();
        spawn_local(async move {
            let mut updates = subscribe_archive::<quantum_link_permissions::QuantumLinkPermissions, _>(
                SubscribeAccountUpdate,
            );
            while let Some(update) = updates.next().await {
                let ui = ui_weak.unwrap();
                if update.account_id != CHANNEL {
                    continue;
                }
                *RELAY_CONTACTED.lock().unwrap() = true;
                ui.global::<BrowserCallbacks>().set_relay_online(true);
                ui.global::<BrowserCallbacks>().set_connection_status("Relay connected".into());

                match serde_json::from_slice::<RelayEnvelope>(&update.update) {
                    Ok(RelayEnvelope::Page { title, url, status, blocks, error, .. }) => {
                        ui.global::<BrowserCallbacks>().set_loading(true);
                        if let Some(err) = error {
                            log::warn!("⚠️ relay error for {}: {}", url, err);
                            set_error(&ui, &format!("relay: {}", err));
                        } else {
                            set_error(&ui, "");
                        }
                        if !title.is_empty() {
                            ui.global::<BrowserCallbacks>().set_page_title(title.into());
                        }
                        if !url.is_empty() {
                            set_url(&ui, &url);
                        }
                        if status == 200 {
                            set_status(&ui, &format!("fetched via relay (HTTP {})", status));
                        } else {
                            set_status(&ui, &format!("HTTP {}", status));
                        }
                        let converted: Vec<Block> = blocks
                            .iter()
                            .map(|b| Block {
                                kind: b.kind.clone(),
                                text: b.text.clone(),
                                href: b.href.clone(),
                            })
                            .collect();
                        render_blocks(&ui, &converted);
                        ui.global::<BrowserCallbacks>().set_loading(false);
                        log::info!("📄 page {} — {} blocks", url, converted.len());
                    }
                    Ok(RelayEnvelope::OpenUrl { url }) => {
                        log::info!("🔀 open-url push: {}", url);
                        request_page(&ui, &url, true);
                        ui.global::<Navigate>().invoke_browse(NavigateOptions {
                            replace: false,
                            animate: Animate::None,
                        });
                    }
                    Err(e) => log::warn!("⚠️ unknown relay envelope: {}", e),
                }
            }
        })
        .detach();
    }

    // ---- Announce the browser to the gateway ----
    send_envelope(serde_json::json!({ "type": "ready", "version": "1.0" }));

    log::info!("✅ BLUETOOTH BROWSER app ready — cold surfaces, relay fetches");
    ui.run().expect("UI running");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_prepends_https_to_bare_domains() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("  example.com  "), "https://example.com");
        assert_eq!(normalize_url("https://example.com/"), "https://example.com/");
        assert_eq!(normalize_url("http://example.com/"), "http://example.com/");
        assert_eq!(normalize_url(""), "");
    }

    #[test]
    fn demo_pages_cover_quick_links() {
        // Every quick link on the home page has an offline demo page.
        assert!(demo_page("https://example.com/").is_some());
        assert!(demo_page("https://www.iana.org/domains/example").is_some());
        assert!(demo_page("https://bitcoin.org/").is_some());
        assert!(demo_page("https://github.com/OZARUMOTO/bluetooth-browser").is_some());
    }

    #[test]
    fn demo_page_blocks_are_safe_kinds_only() {
        let (title, blocks) = demo_page("https://example.com/").unwrap();
        assert!(!title.is_empty());
        assert!(!blocks.is_empty());
        // Only the kinds the device renderer understands may appear.
        for b in &blocks {
            assert!(
                matches!(b.kind.as_str(), "heading" | "text" | "link" | "quote" | "code" | "separator"),
                "unexpected block kind: {}",
                b.kind
            );
            assert!(!b.text.is_empty());
            if b.kind == "link" {
                assert!(b.href.starts_with("https://"));
            }
        }
    }

    #[test]
    fn unknown_urls_have_no_demo_page() {
        assert!(demo_page("https://mempool.space/").is_none());
    }
}
