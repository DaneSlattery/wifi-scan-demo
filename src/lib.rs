#![no_std]

use core::cell::RefCell;

use alloc::{borrow::ToOwned, string::String, vec::Vec};
use defmt::{Format, info};
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    mutex::Mutex,
    signal::Signal,
};
use embassy_time::{Delay, Duration, Timer};
use esp_radio::wifi::{AccessPointInfo, ScanConfig, WifiController};
use serde::{Deserialize, Serialize};

pub mod persistence;
extern crate alloc;

#[derive(Serialize, Deserialize, Default, Debug, Format, Clone)]
pub struct WifiConfig {
    pub bssid: [u8; 6],
    pub ssid: heapless::String<32>,
    pub signal_strength: i8, // store the signal strength
}
impl PartialEq for WifiConfig {
    fn eq(&self, other: &Self) -> bool {
        self.bssid == other.bssid
    }
}

impl WifiConfig {
    const fn new() -> Self {
        Self {
            bssid: [0; 6],
            ssid: heapless::String::new(),
            signal_strength: -100,
        }
    }
}

pub const SSID: &str = env!("SSID");
pub const PASSWORD: &str = env!("PASSWORD");
pub const SSID2: &str = env!("SSID2");
pub const PASSWORD2: &str = env!("PASSWORD2");

pub async fn scan_and_score_wgs(controller: &mut WifiController<'static>) -> Vec<WifiConfig> {
    let scan_conf: ScanConfig<'_> = ScanConfig::default().with_max(10);

    let result = controller.scan_with_config_async(scan_conf).await.unwrap();

    let mut result = result
        .iter()
        .filter(|x| (x.ssid == SSID || x.ssid == SSID2))
        .map(|x| x.to_owned())
        .collect::<Vec<AccessPointInfo>>();

    // the best wifi candidate is that with the highest signal strength,
    result.sort_by(|x, y| {
        // if let Some(n) = &persisted_config
        //     && x.bssid == n.bssid
        // {
        //     return Ordering::Greater;
        // }

        y.signal_strength.cmp(&x.signal_strength)
    });

    for ap in &result {
        // show all aps nearby
        info!(
            "{:?}, {} ,({})",
            ap.ssid.as_str(),
            ap.bssid,
            ap.signal_strength
        );
    }
    let result = result
        .into_iter()
        .map(|f| WifiConfig {
            bssid: f.bssid,
            ssid: f.ssid.as_str().try_into().unwrap(),
            signal_strength: f.signal_strength,
        })
        .collect();
    result
    // Some(&[0u8; 6])
}
