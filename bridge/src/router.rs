//! The routing core: what the bridge does with each message from the device.
//!
//! Device messages arrive as sealed `PassportMessage`s. We only care about
//! one of them in practice:
//!
//! - `AccountUpdate { account_id: "web-0", update: <envelope json> }` — the
//!   browser/dojo-signer envelope. Forward `update` verbatim to the
//!   surf-relay, take its one-line JSON reply, and seal it back to the device
//!   as an `AccountUpdate` on the same channel (exactly what the sim's TCP
//!   path does, just carried over QL).
//!
//! Pairing messages (`PairingRequest`) are answered so the device considers
//! the bridge a legitimate companion.

use std::time::Duration;

use anyhow::{Context, Result};
use foundation_api::bitcoin::AccountUpdate;
use foundation_api::message::{EnvoyMessage, PassportMessage, QuantumLinkMessage};
use foundation_api::quantum_link::{ARIDCache, QuantumLinkIdentity};

use foundation_api::dcbor::CBOREncodable;

use crate::ql::{
    new_envoy_message, seal_and_chunk, unseal_passport, Dechunker, WEB0_CHANNEL,
};
use crate::relay;

pub struct BridgeRouter {
    identity: QuantumLinkIdentity,
    relay_addr: String,
    relay_timeout: Duration,
    arid: ARIDCache,
    dechunker: Dechunker,
}

impl BridgeRouter {
    pub fn new(identity: QuantumLinkIdentity, relay_addr: &str, relay_timeout: Duration) -> Self {
        Self {
            identity,
            relay_addr: relay_addr.to_string(),
            relay_timeout,
            arid: ARIDCache::new(),
            dechunker: Dechunker::default(),
        }
    }

    /// Build a sealed `PairingRequest` to send to a device whose XID we know
    /// (from scanning its pairing QR). The device's QL server auto-accepts
    /// this and replies `PairingResponse`; after that, web-0 traffic flows.
    pub fn build_pairing_request(
        &self,
        device_xid: &foundation_api::bc_xid::XIDDocument,
        device_name: &str,
    ) -> Result<Vec<Vec<u8>>> {
        let request = foundation_api::message::QuantumLinkMessage::PairingRequest(
            foundation_api::pairing::PairingRequest {
                xid_document: self.identity.xid_document.to_cbor_data(),
                device_name: device_name.to_string(),
            },
        );
        let envelope = new_envoy_message(request);
        seal_and_chunk(envelope, &self.identity, device_xid)
    }

    /// Feed one raw BLE frame. When a complete message is reassembled and
    /// processed, returns the sealed reply frames to write back (if any).
    pub fn on_frame(&mut self, frame: &[u8]) -> Result<Vec<Vec<u8>>> {
        let Some(envelope_cbor) = self.dechunker.push(frame)? else {
            return Ok(Vec::new()); // still awaiting more chunks
        };
        let (passport_msg, sender) =
            unseal_passport(&envelope_cbor, &self.identity, &mut self.arid)
                .context("unseal passport message")?;
        self.route(passport_msg, &sender)
    }

    fn route(
        &mut self,
        msg: PassportMessage,
        sender: &foundation_api::bc_xid::XIDDocument,
    ) -> Result<Vec<Vec<u8>>> {
        match msg.message {
            QuantumLinkMessage::AccountUpdate(update) => {
                if update.account_id != WEB0_CHANNEL {
                    log::debug!("ignoring AccountUpdate on channel {:?}", update.account_id);
                    return Ok(Vec::new());
                }
                self.handle_web0(update.update, sender)
            }
            QuantumLinkMessage::PairingRequest(req) => {
                // The device wants to pair with us — same auto-accept the
                // KeyOS server does for incoming requests. Reply with a
                // matching PairingResponse once we know the device XID.
                log::info!(
                    "pairing request from device {:?} (name {:?})",
                    sender,
                    req.device_name
                );
                let response = foundation_api::message::QuantumLinkMessage::PairingResponse(
                    foundation_api::pairing::PairingResponse {
                        passport_model: foundation_api::passport::PassportModel::Prime,
                        passport_firmware_version: foundation_api::passport::PassportFirmwareVersion(
                            String::new(),
                        ),
                        passport_serial: foundation_api::passport::PassportSerial(String::new()),
                        passport_color: foundation_api::passport::PassportColor::Dark,
                        onboarding_complete: false,
                        device_name: Some(String::from("surf-bridge")),
                    },
                );
                let envelope = new_envoy_message(response);
                seal_and_chunk(envelope, &self.identity, sender)
            }
            QuantumLinkMessage::Heartbeat(_) => {
                // Keepalive — echo one back so the device keeps the link.
                log::trace!("heartbeat");
                let envelope = new_envoy_message(QuantumLinkMessage::Heartbeat(
                    foundation_api::status::Heartbeat {},
                ));
                seal_and_chunk(envelope, &self.identity, sender)
            }
            other => {
                log::debug!("ignoring device message {other:?}");
                Ok(Vec::new())
            }
        }
    }

    fn handle_web0(&mut self, envelope: Vec<u8>, sender: &foundation_api::bc_xid::XIDDocument) -> Result<Vec<Vec<u8>>> {
        log::debug!(
            "web-0 envelope: {}",
            String::from_utf8_lossy(&envelope).trim()
        );
        let reply = relay::exchange(&self.relay_addr, &envelope, self.relay_timeout)
            .context("surf-relay exchange")?;
        log::debug!("relay reply: {}", String::from_utf8_lossy(&reply).trim());
        let reply_msg = EnvoyMessage {
            message: QuantumLinkMessage::AccountUpdate(AccountUpdate {
                account_id: WEB0_CHANNEL.to_string(),
                update: reply,
            }),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0),
            protocol_version: Some(1),
        };
        seal_and_chunk(reply_msg, &self.identity, sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundation_api::message::QuantumLinkMessage;
    use foundation_api::quantum_link::{QuantumLink, QuantumLinkIdentity};

    /// The device side must be able to unseal our PairingRequest with its
    /// own keys and read our XID + name — proving the init flow works.
    #[test]
    fn pairing_request_is_unsealable_by_device() {
        let bridge = QuantumLinkIdentity::generate();
        let device = QuantumLinkIdentity::generate();
        let router = BridgeRouter::new(
            bridge.clone(),
            "127.0.0.1:1",
            Duration::from_millis(100),
        );
        let frames = router
            .build_pairing_request(&device.xid_document, "surf-bridge")
            .unwrap();

        // Reassemble on the device side.
        let mut dechunker = crate::ql::Dechunker::default();
        let mut complete = None;
        for f in frames {
            if let Some(c) = dechunker.push(&f).unwrap() {
                complete = Some(c);
            }
        }
        let cbor = foundation_api::dcbor::CBOR::try_from_data(&complete.unwrap()).unwrap();
        let envelope = foundation_api::bc_envelope::Envelope::try_from_cbor(cbor).unwrap();
        let (msg, sender) =
            foundation_api::message::EnvoyMessage::unseal_envoy_message_with_replay_check(
                &envelope,
                device.private_keys.as_ref().unwrap(),
                &mut ARIDCache::new(),
            )
            .unwrap();
        assert_eq!(sender, bridge.xid_document, "sender is the bridge");
        match msg.message {
            QuantumLinkMessage::PairingRequest(req) => {
                assert_eq!(req.device_name, "surf-bridge");
                // req.xid_document is our XID as CBOR bytes — parse it back.
                let xid_cbor =
                    foundation_api::dcbor::CBOR::try_from_data(&req.xid_document).unwrap();
                let xid = foundation_api::bc_xid::XIDDocument::try_from(xid_cbor).unwrap();
                assert_eq!(xid, bridge.xid_document);
            }
            other => panic!("expected PairingRequest, got {other:?}"),
        }
    }
}
