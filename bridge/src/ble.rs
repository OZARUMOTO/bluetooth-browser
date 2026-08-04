//! BLE transport: Nordic UART Service (NUS) via `btleplug`.
//!
//! The Passport Prime advertises the standard Nordic UART Service:
//! - Service `6E400001-B5A3-F393-E0A9-E50E24DCCA9E`
//! - RX char `6E400002-...` (write_without_response — bridge → device)
//! - TX char `6E400003-...` (notify — device → bridge)
//!
//! Same service Envoy's Android `QLConnection.kt` uses (PRIME_SERVICE_UUID),
//! confirmed against `prime-ble-firmware/firmware/src/nus.rs`.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use btleplug::api::{
    Central, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Peripheral};
use futures::stream::StreamExt;
use uuid::Uuid;

pub const NUS_SERVICE_UUID: Uuid = Uuid::from_u128(0x6E400001_B5A3_F393_E0A9_E50E24DCCA9E);
pub const NUS_RX_CHAR_UUID: Uuid = Uuid::from_u128(0x6E400002_B5A3_F393_E0A9_E50E24DCCA9E);
pub const NUS_TX_CHAR_UUID: Uuid = Uuid::from_u128(0x6E400003_B5A3_F393_E0A9_E50E24DCCA9E);

/// Discover the Passport. If `mac` is given (e.g. `DC:85:DE:F0:82:EB`-style
/// is the box's own adapter, so the *device* has its own), filter by address;
/// otherwise pick the first peripheral advertising NUS.
pub async fn find_passport(
    adapter: &Adapter,
    mac: Option<&str>,
    scan_seconds: u64,
) -> Result<Peripheral> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start scan")?;
    tokio::time::sleep(Duration::from_secs(scan_seconds)).await;

    let peripherals = adapter.peripherals().await.context("list peripherals")?;
    for p in &peripherals {
        let props = p.properties().await.ok().flatten();
        if let Some(m) = mac {
            let addr = props
                .as_ref()
                .map(|pr| pr.address.to_string())
                .unwrap_or_default();
            if addr.eq_ignore_ascii_case(m) {
                adapter.stop_scan().await.ok();
                return Ok(p.clone());
            }
        } else if let Some(pr) = props {
            let name = pr.local_name.clone().unwrap_or_default();
            let has_nus = pr
                .services
                .iter()
                .any(|s| *s == NUS_SERVICE_UUID);
            if has_nus || name.to_ascii_lowercase().contains("passport") {
                log::info!("found device: name={name:?} addr={}", pr.address);
                adapter.stop_scan().await.ok();
                return Ok(p.clone());
            }
        }
    }
    adapter.stop_scan().await.ok();
    Err(anyhow!("no Passport found (scanned {}s)", scan_seconds))
}

pub struct NusSession {
    peripheral: Peripheral,
    rx_char: btleplug::api::Characteristic,
    tx_char: btleplug::api::Characteristic,
}

impl NusSession {
    pub async fn connect(peripheral: &Peripheral) -> Result<Self> {
        peripheral.connect().await.context("connect")?;
        peripheral.discover_services().await.context("discover services")?;

        let chars = peripheral.characteristics();
        let rx_char = chars
            .iter()
            .find(|c| c.uuid == NUS_RX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("NUS RX characteristic not found"))?;
        let tx_char = chars
            .iter()
            .find(|c| c.uuid == NUS_TX_CHAR_UUID)
            .cloned()
            .ok_or_else(|| anyhow!("NUS TX characteristic not found"))?;

        Ok(Self {
            peripheral: peripheral.clone(),
            rx_char,
            tx_char,
        })
    }

    /// Write one raw frame to the device (write_without_response).
    pub async fn write(&self, frame: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.rx_char, frame, WriteType::WithoutResponse)
            .await
            .context("write NUS RX")?;
        Ok(())
    }

    /// Enable notifications on the TX char and return the frame stream
    /// (boxed so it is `Unpin` for `.next()` in the main loop).
    pub async fn notifications(
        &self,
    ) -> Result<futures::stream::BoxStream<'static, Vec<u8>>> {
        self.peripheral
            .subscribe(&self.tx_char)
            .await
            .context("subscribe NUS TX")?;
        let stream = self
            .peripheral
            .notifications()
            .await
            .context("open notification stream")?;
        Ok(stream
            .filter_map(|n| async move { Some(n.value) })
            .boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_match_foundation_firmware() {
        // Cross-checked against prime-ble-firmware/firmware/src/nus.rs and
        // envoy/android/.../ble/QLConnection.kt.
        assert_eq!(
            NUS_SERVICE_UUID.to_string(),
            "6e400001-b5a3-f393-e0a9-e50e24dcca9e"
        );
        assert_eq!(
            NUS_RX_CHAR_UUID.to_string(),
            "6e400002-b5a3-f393-e0a9-e50e24dcca9e"
        );
        assert_eq!(
            NUS_TX_CHAR_UUID.to_string(),
            "6e400003-b5a3-f393-e0a9-e50e24dcca9e"
        );
    }
}
