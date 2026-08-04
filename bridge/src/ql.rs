//! Quantum Link encoding layer.
//!
//! Mirrors Envoy's `foundation_api` wrapper (`packages/foundation_api/rust/
//! src/api/ql.rs`): seal an `EnvoyMessage` into a bc-envelope sealed to the
//! device's XID, BTP-chunk it into wire frames; and the inverse — dechunk
//! incoming frames and unseal a `PassportMessage` with replay protection.
//!
//! This is the *exact same* crypto the phone companion runs; the bridge is
//! byte-for-byte wire compatible with the Passport.

use anyhow::{anyhow, Context, Result};
use btp::{chunk, Chunk, MasterDechunker};
use foundation_api::bc_envelope::Envelope;
use foundation_api::bc_xid::XIDDocument;
use foundation_api::dcbor::{CBOR, CBOREncodable};
use foundation_api::message::{EnvoyMessage, PassportMessage, PROTOCOL_VERSION};
use foundation_api::quantum_link::{ARIDCache, QuantumLink, QuantumLinkIdentity};

/// Wire channel for the BLUETOOTH BROWSER envelopes (matches the KeyOS app:
/// `const CHANNEL: &str = "web-0";`).
pub const WEB0_CHANNEL: &str = "web-0";

/// Seal an outgoing message to the device and BTP-chunk the result.
pub fn seal_and_chunk(
    message: EnvoyMessage,
    identity: &QuantumLinkIdentity,
    recipient: &XIDDocument,
) -> Result<Vec<Vec<u8>>> {
    let envelope = QuantumLink::seal(
        message,
        (
            identity.private_keys.as_ref().expect("identity has keys"),
            &identity.xid_document,
        ),
        recipient,
    );
    let cbor = envelope.to_cbor_data();
    Ok(chunk(&cbor).map(|c| c.to_vec()).collect())
}

/// Streaming dechunker for incoming BLE frames → complete sealed envelopes.
pub struct Dechunker {
    inner: MasterDechunker<10>,
}

impl Default for Dechunker {
    fn default() -> Self {
        Self {
            inner: MasterDechunker::default(),
        }
    }
}

impl Dechunker {
    /// Feed one raw BLE frame. Returns the complete envelope CBOR once all
    /// chunks of a message have arrived, else `None`.
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        let chunk = Chunk::decode(frame).context("bad BTP chunk")?;
        match self.inner.insert_chunk(chunk) {
            Some(data) => Ok(Some(data)),
            None => Ok(None),
        }
    }
}

/// Unseal a complete incoming envelope into a `PassportMessage`, verifying
/// the sender XID and replay-protecting via ARID.
pub fn unseal_passport(
    envelope_cbor: &[u8],
    identity: &QuantumLinkIdentity,
    arid: &mut ARIDCache,
) -> Result<(PassportMessage, XIDDocument)> {
    let cbor = CBOR::try_from_data(envelope_cbor).context("invalid cbor")?;
    let envelope = Envelope::try_from_cbor(cbor).context("invalid envelope")?;
    PassportMessage::unseal_passport_message_with_replay_check(
        &envelope,
        identity.private_keys.as_ref().expect("identity has keys"),
        arid,
    )
    .map_err(|e| anyhow!("unseal failed: {e:?}"))
}

/// Build a fresh `EnvoyMessage` timestamped now (matches Envoy's wrapper).
pub fn new_envoy_message(message: foundation_api::message::QuantumLinkMessage) -> EnvoyMessage {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    EnvoyMessage {
        message,
        timestamp: now,
        protocol_version: Some(PROTOCOL_VERSION),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundation_api::bitcoin::AccountUpdate;
    use foundation_api::message::QuantumLinkMessage;

    #[test]
    fn seal_dechunk_unseal_round_trip() {
        // Companion (bridge) and device identities, like the real pair.
        let bridge = QuantumLinkIdentity::generate();
        let device = QuantumLinkIdentity::generate();

        // The device sends an AccountUpdate on web-0 (a fetch envelope).
        let device_message = PassportMessage {
            message: QuantumLinkMessage::AccountUpdate(AccountUpdate {
                account_id: WEB0_CHANNEL.to_string(),
                update: br#"{"type":"fetch","id":"1","url":"https://example.com/"}"#.to_vec(),
            }),
            status: foundation_api::status::DeviceStatus {
                version: "0.1.0".into(),
                battery_level: 88,
            },
            protocol_version: Some(PROTOCOL_VERSION),
        };
        // Device seals to the bridge (the bridge's XID is the recipient).
        let sealed = QuantumLink::seal(
            device_message.clone(),
            (device.private_keys.as_ref().unwrap(), &device.xid_document),
            &bridge.xid_document,
        );
        let cbor = sealed.to_cbor_data();

        // Break into chunks and feed through the dechunker.
        let frames: Vec<Vec<u8>> = chunk(&cbor).map(|c| c.to_vec()).collect();
        assert!(frames.len() > 0, "envelope should produce >= 1 chunk");
        let mut dechunker = Dechunker::default();
        let mut reassembled = None;
        for f in &frames {
            if let Some(complete) = dechunker.push(f).unwrap() {
                reassembled = Some(complete);
            }
        }
        let reassembled = reassembled.expect("dechunker must reassemble");

        // Bridge unseals with its own keys.
        let mut arid = ARIDCache::new();
        let (msg, sender) =
            unseal_passport(&reassembled, &bridge, &mut arid).expect("unseal ok");
        assert_eq!(sender, device.xid_document, "sender XID is the device");
        match msg.message {
            QuantumLinkMessage::AccountUpdate(update) => {
                assert_eq!(update.account_id, WEB0_CHANNEL);
                assert_eq!(
                    update.update,
                    br#"{"type":"fetch","id":"1","url":"https://example.com/"}"#.to_vec()
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // Replay protection: feeding the same envelope again must fail.
        let cbor2 = CBOR::try_from_data(&reassembled).unwrap();
        let env2 = Envelope::try_from_cbor(cbor2).unwrap();
        let replay = PassportMessage::unseal_passport_message_with_replay_check(
            &env2,
            bridge.private_keys.as_ref().unwrap(),
            &mut arid,
        );
        assert!(replay.is_err(), "replay must be rejected");
    }

    #[test]
    fn seal_and_chunk_matches_dechunker() {
        let bridge = QuantumLinkIdentity::generate();
        let device = QuantumLinkIdentity::generate();
        let msg = new_envoy_message(QuantumLinkMessage::AccountUpdate(AccountUpdate {
            account_id: WEB0_CHANNEL.to_string(),
            update: br#"{"type":"ping"}"#.to_vec(),
        }));
        let frames = seal_and_chunk(msg, &bridge, &device.xid_document).unwrap();
        let mut dechunker = Dechunker::default();
        let mut complete = None;
        for f in frames {
            if let Some(c) = dechunker.push(&f).unwrap() {
                complete = Some(c);
            }
        }
        assert!(complete.is_some());
    }
}
