#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::cell::{Ref, RefCell};
use core::cmp::Ordering;
use core::net::Ipv4Addr;
use core::result;

use alloc::borrow::ToOwned;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::{self, Vec};
use anyhow::Error;
use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::select;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, StackResources};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex, RawMutex};
use embassy_sync::channel::Receiver;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, WithTimeout};
use embedded_io::Read;
use esp_bootloader_esp_idf::partitions::{self, FlashRegion};
use esp_hal::peripherals::{self, Peripherals, WIFI};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::{clock::CpuClock, rng::Rng};
use esp_radio::wifi::{
    AccessPointInfo, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent,
};
use esp_radio::{
    Controller,
    wifi::{self, ClientConfig},
};
use esp_rtos::embassy;
use esp_storage::FlashStorage;
use ieee80211::{match_frames, mgmt_frame::BeaconFrame};
use serde::{Deserialize, Serialize};
use wifi_scan_demo::persistence::{LOAD_WIFI, STORE_WIFI, persistence};
use wifi_scan_demo::{KNOWN_CREDS, WifiConfig, scan_and_score_wgs};
use {esp_backtrace as _, esp_println as _};

use embedded_storage::{ReadStorage, Storage};

// use wifi_scan_demo::{WIFI_STARTED, scanner};
extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// the system works as follows:
// 1. start persistence loop
//    - persistence loop will read NVS and fetch most recent connected access point, by bssid
//      if one is not found, take the default as the starting config.
//      Persistence then posts to a signal to notify other tasks that the wifi config is loaded
// 2. start connect loop
//    - connect loop will wait for persistence signal and then use those credentials to initialise the wifi
//    - on first boot:
//       - start wifi
//       - connect loop will then scan up to 10 nearby AP's matching either of the known ssids
//       - it will rank the nearby APs by signal strength (higher is better)
//       - if the persisted config is there, it will rank that highest, so attempt that first
//       - we can then go down the list of APs starting at the highest rank,
//         - attempt to connect
//           - if connection fails, try the next AP
//           - if connection succeeds, the main loop will ping the web.
//           - one could likely integrate this ping behaviour

/// used by the main loop to notify the connection state machine if this WG connected
/// true when connected
/// false when not connected
pub static WG_CONNECT_STATUS: Signal<CriticalSectionRawMutex, bool> = Signal::new();
pub static SCAN_CMD: Signal<CriticalSectionRawMutex, ()> = Signal::new();
pub static SCAN_COMPLETE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub static DISCONNECT_DETECTED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub static CANDIDATES: Mutex<CriticalSectionRawMutex, RefCell<Vec<WifiConfig>>> =
    Mutex::new(RefCell::new(Vec::new()));

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 0.6.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 98767);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Embassy initialized!");

    let radio_init = &*mk_static!(
        Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );

    let (mut _wifi_controller, _interfaces) =
        esp_radio::wifi::new(&radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    let wifi_interface = _interfaces.sta;

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    // spawn other threads
    spawner.spawn(persistence(peripherals.FLASH)).ok();

    let mut persisted_config = LOAD_WIFI.wait().await;
    spawner
        .spawn(wifi_mgr(_wifi_controller, persisted_config.clone()))
        .ok();
    spawner.spawn(best_connection_task(persisted_config)).ok();

    spawner.spawn(net_task(runner)).ok();
    // spawner.spawn(very_busy_loop()).ok();

    // todo: consider moving into separate task
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    // the main loop is as follows
    // wait for link up
    //  when up, wait for dhcp assignment
    //   when assigned,
    loop {
        if !stack.is_link_up() {
            // wait for link up
            Timer::after(Duration::from_millis(500)).await;
        }
        // link is up

        'link_loop: loop {
            if let Some(config) = stack.config_v4() {
                info!("Got IP: {:#}", config.address);

                'socket_loop: loop {
                    Timer::after(Duration::from_secs(1)).await;
                    info!("Hello world!");
                    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

                    socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

                    // 1.1.1.1:80, if we can connect, we're good
                    let remote_endpoint = (Ipv4Addr::new(1, 1, 1, 1), 80);

                    info!("Connecting...");

                    let r = socket.connect(remote_endpoint).await;

                    if let Err(e) = r {
                        info!("connect error: {:?}", e);
                        WG_CONNECT_STATUS.signal(false);
                        break 'link_loop;
                    } else {
                        info!("Socket connected");
                        WG_CONNECT_STATUS.signal(true);
                    }
                    Timer::after(Duration::from_millis(3000)).await;
                }
            } else {
                info!("Waiting to get ip addr");

                Timer::after(Duration::from_millis(5000)).await;
            }
        }

        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Something went terribly wrong");
    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0-rc.1/examples/src/bin
}

/// we use the bssid to identify a specific WG, as multiple will advertise on same ssid
fn get_client_config_from_candidate(wifi: &WifiConfig) -> ClientConfig {
    if wifi.ssid == KNOWN_CREDS.0.ssid {
        ClientConfig::default()
            .with_ssid(KNOWN_CREDS.0.ssid.into())
            .with_bssid(wifi.bssid)
            .with_password(KNOWN_CREDS.0.password.into())
    } else {
        ClientConfig::default()
            .with_ssid(KNOWN_CREDS.1.ssid.into())
            .with_bssid(wifi.bssid)
            .with_password(KNOWN_CREDS.1.password.into())
    }
}

enum WifiRequest {
    Connect {
        conf: WifiConfig,
    },
    Scan {
        resp: oneshot::Sender<Vec<WifiConfig>>,
    },
}

// actively searches for the best connection
#[embassy_executor::task]
async fn best_connection_task(persisted_config: Option<WifiConfig>) -> ! {
    // persistence will load the previous connection from flash, if any

    let mut local_persisted = persisted_config.clone();
    // on first boot, scan nearby wifis
    SCAN_CMD.signal(());

    let mut new_best_found = false;
    loop {
        if SCAN_COMPLETE.signaled() {
            SCAN_COMPLETE.wait().await;
            let candidates = CANDIDATES.lock().await;
            let candidate_ref = candidates.borrow();
            let best_candidate = candidate_ref.first();
            info!("Scan complete, best = {}", best_candidate);
            match (best_candidate, &local_persisted) {
                (None, None) => {
                    // no candidates and no persisted
                }
                (None, Some(x)) => {
                    // no candidates, persisted still better
                }
                (Some(c), None) => {
                    // a new winner emerges
                    STORE_WIFI.signal(c.clone());
                    local_persisted = Some(c.clone());
                    new_best_found = true;
                }
                (Some(c), Some(p)) => {
                    if c == p {
                        // same as persisted,
                        new_best_found = true;
                    }
                    if c > p {
                        STORE_WIFI.signal(c.clone());
                        local_persisted = Some(c.clone());
                        new_best_found = true;
                    }
                }
            }
        }

        {
            match esp_radio::wifi::sta_state() {
                wifi::WifiStaState::Connected => {
                    // scan once an hour if we haven't found a new best
                    if !new_best_found {
                        match select::select(
                            Timer::after(Duration::from_secs(60 * 60)),
                            DISCONNECT_DETECTED.wait(),
                        )
                        .await
                        {
                            select::Either::First(_) => SCAN_CMD.signal(()),
                            select::Either::Second(_) => {} // break,
                        }
                    }
                }
                wifi::WifiStaState::Disconnected => {
                    // scan once every 5 minutes if we are currently chronically disconnected
                    Timer::after(Duration::from_secs(5 * 60)).await;
                    SCAN_CMD.signal(());
                }
                _ => {}
            }
        }
        Timer::after(Duration::from_secs(10)).await
    }
}

#[embassy_executor::task]
async fn wifi_mgr(
    mut controller: WifiController<'static>,
    persisted_config: Option<WifiConfig>,
) -> ! {
    info!("Start wifi mgr task");
    info!("Device Capabilities: {:?}", controller.capabilities());

    let default_config = if let Some(persist) = persisted_config {
        get_client_config_from_candidate(&persist)
    } else {
        ClientConfig::default()
            .with_ssid(KNOWN_CREDS.0.ssid.into())
            .with_password(KNOWN_CREDS.0.password.into())
    };

    let client_config = ModeConfig::Client(default_config.clone());

    controller.set_config(&client_config).unwrap();

    info!("Starting wifi");
    controller.start_async().await.unwrap();
    info!("Started wifi");

    loop {
        match esp_radio::wifi::sta_state() {
            wifi::WifiStaState::Connected => {
                run_connected(&mut controller).await;
            }

            _ => run_disconnected(&mut controller).await,
        }
        Timer::after(Duration::from_millis(3000)).await
    }
}

async fn run_disconnected(controller: &mut WifiController<'static>) {
    // we're currently disconnected
    if SCAN_CMD.signaled() {
        // clear signal
        SCAN_CMD.wait().await;
        do_scan(controller).await
    }
    info!("Currently disconnected");
    // pick best next candidate
    let candidates = CANDIDATES.lock().await;
    let mut candidates_mut = candidates.borrow_mut();
    if let Some(best) = candidates_mut.first() {
        controller
            .set_config(&ModeConfig::Client(get_client_config_from_candidate(best)))
            .unwrap();
        info!("Attempting to connect to {}", best);
    }
    match controller.connect_async().await {
        Ok(_) => {
            if let Some(best) = candidates_mut.first_mut() {
                best.connect_success = Some(true);
            }
            info!("Wifi Connected!");
        }
        Err(err) => {
            if let Some(best) = candidates_mut.first_mut() {
                best.connect_success = Some(false);
            }
            info!("Failed to connect to wifi {:?}", err);
        }
    }
}

async fn run_connected(
    controller: &mut WifiController<'static>,
    // candidates: &mut [WifiConfig; 10],
) {
    info!("Connected, waiting for disconnect or scan");
    let disconnect_evt = controller.wait_for_event(WifiEvent::StaDisconnected);

    let scan_event = SCAN_CMD.wait();

    match select::select(disconnect_evt, scan_event).await {
        select::Either::First(_) => {
            // we're disconnected, pick the next gateway
            let candidates = CANDIDATES.lock().await;
            // candidates.borrow_mut().first_mut()
            let mut candidates_mut = candidates.borrow_mut();
            if let Some(old_best) = candidates_mut.first_mut() {
                old_best.connect_success = Some(false);
            }
            candidates_mut.sort_by(|x, y| x.cmp(y));
            DISCONNECT_DETECTED.signal(());
            // new best
        }
        select::Either::Second(_) => {
            do_scan(controller).await;
        }
    }
}

async fn do_scan(controller: &mut WifiController<'static>) {
    let mut wg = scan_and_score_wgs(controller).await;
    let candidates = CANDIDATES.lock().await;
    let mut candidates_mut = candidates.borrow_mut();

    for w in &mut wg {
        match candidates_mut.binary_search_by_key(&w.bssid, |w| w.bssid) {
            Ok(x) => w.connect_success = candidates_mut[x].connect_success,
            Err(_) => {}
        }
    }
    // replace candidates
    wg.sort_by(|x, y| x.cmp(y));
    *candidates_mut = wg;

    SCAN_COMPLETE.signal(());
}

/// this can be enabled to show that our very busy loop can still run at a decent rate
#[embassy_executor::task]
async fn very_busy_loop() {
    loop {
        info!("-");
        Timer::after(Duration::from_millis(20)).await
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
