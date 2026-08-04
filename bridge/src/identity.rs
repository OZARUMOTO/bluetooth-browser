//! Bridge identity persistence.
//!
//! The bridge holds its own `QuantumLinkIdentity` (XID document + private
//! keys) so the Passport can pair with *the box* exactly as it would pair
//! with Envoy. We persist it to disk so pairing survives reboots.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use foundation_api::bc_xid::XIDDocument;
use foundation_api::dcbor::{CBOR, CBOREncodable};
use foundation_api::quantum_link::QuantumLinkIdentity;

const STATE_FILE: &str = "surf-bridge-state.bin";

#[derive(Debug, Clone)]
pub struct BridgeState {
    /// Our companion identity (XID + private keys).
    pub identity: QuantumLinkIdentity,
    /// The paired Passport's XID document (device side of the link).
    pub device_xid: Option<XIDDocument>,
}

impl BridgeState {
    pub fn new(identity: QuantumLinkIdentity) -> Self {
        Self {
            identity,
            device_xid: None,
        }
    }

    /// Load state from `dir`, or generate a fresh identity if none exists.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_FILE);
        if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read {}", path.display()))?;
            Self::from_bytes(&bytes)
        } else {
            let state = Self::new(QuantumLinkIdentity::generate());
            state.save(dir)?;
            Ok(state)
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        // The state file holds our QL private keys — keep it private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(STATE_FILE);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut map = foundation_api::dcbor::prelude::Map::new();
        map.insert(
            CBOR::from("identity"),
            CBOR::from(foundation_api::dcbor::ByteString::from(self.identity.to_bytes())),
        );
        if let Some(xid) = &self.device_xid {
            map.insert(
                CBOR::from("device_xid"),
                CBOR::from(foundation_api::dcbor::ByteString::from(xid.to_cbor_data())),
            );
        }
        CBOR::from(map).to_cbor_data()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let cbor = CBOR::try_from_data(bytes)
            .map_err(|e| anyhow!("invalid state cbor: {e:?}"))?;
        let case = cbor.into_case();
        let foundation_api::dcbor::CBORCase::Map(map) = case else {
            bail!("state file is not a map");
        };
        let identity_bytes: Vec<u8> = map
            .get::<&str, foundation_api::dcbor::ByteString>("identity")
            .ok_or_else(|| anyhow!("missing identity"))?
            .into();
        let identity = QuantumLinkIdentity::from_bytes(&identity_bytes)
            .map_err(|e| anyhow!("corrupt identity: {e:?}"))?;

        let device_xid = match map.get::<&str, foundation_api::dcbor::ByteString>("device_xid") {
            Some(raw) => {
                let raw: Vec<u8> = raw.into();
                let cbor = CBOR::try_from_data(&raw)
                    .map_err(|e| anyhow!("invalid device xid cbor: {e:?}"))?;
                Some(
                    XIDDocument::try_from(cbor)
                        .map_err(|e| anyhow!("invalid device xid: {e:?}"))?,
                )
            }
            None => None,
        };
        Ok(Self {
            identity,
            device_xid,
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_and_without_device() {
        let dir = std::env::temp_dir().join(format!("surf-bridge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Fresh identity, no device paired yet.
        let a = BridgeState::load_or_create(&dir).unwrap();
        assert!(a.device_xid.is_none());
        let reloaded = BridgeState::load_or_create(&dir).unwrap();
        assert!(reloaded.device_xid.is_none());
        assert_eq!(
            reloaded.identity.xid_document,
            a.identity.xid_document,
            "identity must survive reload"
        );

        // Simulate a paired device: create a second identity and treat its
        // XID as the device's.
        let device = QuantumLinkIdentity::generate();
        let mut paired = a.clone();
        paired.device_xid = Some(device.xid_document.clone());
        paired.save(&dir).unwrap();
        let reloaded2 = BridgeState::load_or_create(&dir).unwrap();
        assert!(reloaded2.device_xid.is_some());
        assert_eq!(
            reloaded2.device_xid.unwrap(),
            device.xid_document,
            "paired device XID must survive reload"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
