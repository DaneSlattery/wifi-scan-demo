#![no_std]

use core::{cell::RefCell, cmp::Ordering};

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

// Represents a candidate wifi connection
#[derive(Serialize, Deserialize, Default, Debug, Format, Clone, Eq, PartialOrd)]
pub struct WifiConfig {
    pub bssid: [u8; 6],
    pub ssid: heapless::String<32>,
    pub signal_strength: i8,
    // set if/when we ever use this candidate
    pub connect_success: Option<bool>,
}

impl WifiConfig {
    fn cmp_ss(&self, other: &Self) -> core::cmp::Ordering {
        return self.signal_strength.cmp(&other.signal_strength);
    }
}
impl PartialEq for WifiConfig {
    fn eq(&self, other: &Self) -> bool {
        self.bssid == other.bssid
    }
}

impl Ord for WifiConfig {
    fn cmp(&self, other: &Self) -> Ordering {
        // a wifi config
        match (self.connect_success, other.connect_success) {
            (Some(true), Some(true)) => {
                // both configs connected, better signal wins
                return Self::cmp_ss(&self, other);
            }
            (Some(true), Some(false)) => {
                // self connected, we're better
                return core::cmp::Ordering::Greater;
            }
            (Some(false), Some(true)) => {
                // other connected, self didn't, it's better
                return Ordering::Less;
            }
            (Some(false), Some(false)) => {
                // neither connected, better signals wins
                return Self::cmp_ss(&self, other);
            }
            (None, None) => {
                // never been used
                return Self::cmp_ss(&self, other);
            }
            (None, Some(true)) => {
                // self never been used, other connected, it's better
                return Ordering::Less;
            }
            (None, Some(false)) => {
                // self never been used, other didn't connect, we're better
                return Ordering::Greater;
            }
            (Some(x), None) => {
                match x {
                    true => {
                        // self been used, and it connected, we're better
                        return Ordering::Greater;
                    }
                    false => {
                        // self been used, and it didn't connect, rather use other
                        return Ordering::Less;
                    }
                }
            }
        }
    }
}

// represents credentials baked into firmware
pub struct Credential {
    pub ssid: &'static str,
    pub password: &'static str,
}

pub const KNOWN_CREDS: (Credential, Credential) = (
    Credential {
        ssid: SSID,
        password: PASSWORD,
    },
    Credential {
        ssid: PASSWORD,
        password: PASSWORD2,
    },
);

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const SSID2: &str = env!("SSID2");
const PASSWORD2: &str = env!("PASSWORD2");

const SCAN_COUNT: usize = 10;

pub async fn scan_and_score_wgs(controller: &mut WifiController<'static>) -> Vec<WifiConfig> {
    // worst case scan time 20ms*SCAN_COUNT
    let scan_conf: ScanConfig<'_> = ScanConfig::default().with_max(SCAN_COUNT);
    let result = controller.scan_with_config_async(scan_conf).await.unwrap();

    let mut result = result
        .iter()
        .filter(|x| (x.ssid == SSID || x.ssid == SSID2))
        .map(|x| x.to_owned())
        .map(|x| WifiConfig {
            bssid: x.bssid,
            ssid: x.ssid.as_str().try_into().unwrap(),
            signal_strength: x.signal_strength,
            connect_success: None,
        })
        .collect::<Vec<WifiConfig>>();

    // the best wifi candidate will sort to the top, check the Ord impl for
    // how they're picked
    result.sort_by(|x, y| y.cmp(&x));

    for ap in &result {
        // show all aps nearby
        info!(
            "{:?}, {} ,({})",
            ap.ssid.as_str(),
            ap.bssid,
            ap.signal_strength
        );
    }

    result
}
