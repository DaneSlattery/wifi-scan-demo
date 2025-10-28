#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::cell::RefCell;
use core::cmp::Ordering;
use core::net::Ipv4Addr;
use core::result;

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use alloc::vec::{self, Vec};
use anyhow::Error;
use defmt::info;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, StackResources};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
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
//              - if pinging the web fails, it will send a disconnect signal
//           - if we get the disconnect signal, try the next AP
//       -
//

/// used by the main loop to notify the connection state machine if this WG connected
/// true when connected
/// false when not connected
pub static WG_CONNECT_STATUS: Signal<CriticalSectionRawMutex, bool> = Signal::new();

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
    spawner.spawn(connection(_wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();

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

                Timer::after(Duration::from_millis(500)).await;
            }
        }

        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Something went terribly wrong");
    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0-rc.1/examples/src/bin
}

// #[embassy_executor::task]
// async fn scanner( controller: RefCell<WifiController<'static>>) -> ! {
//     loop {
//         //scan wgs
//         let mut controller = controller.borrow_mut();
//         scan_and_score_wgs(&mut controller).await;
//     }
// }

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

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) -> ! {
    use alloc::vec;
    info!("Start connection task");
    info!("Device Capabilities: {:?}", controller.capabilities());

    let default_config: ClientConfig = ClientConfig::default()
        .with_ssid(KNOWN_CREDS.0.ssid.into())
        .with_password(KNOWN_CREDS.0.password.into());

    // persistence will load the previous connection from flash, if any
    let persisted_config = LOAD_WIFI.wait().await;
    let mut current_candidate: Option<WifiConfig> = None;

    let mut connection_queue: Vec<WifiConfig> = vec![];

    // connection state machine
    loop {
        match esp_radio::wifi::sta_state() {
            wifi::WifiStaState::Connected => {
                // todo: wait for disconnect event or check gateway status again
                // a natural disconnect is okay, we can probably just try again
                let natural_disconnect = controller.wait_for_event(WifiEvent::StaDisconnected);
                // a signaled disconnect is the other loop informing that this WG doesn't provide reliable internet
                let signal_disconnect = WG_CONNECT_STATUS.wait();
                match embassy_futures::select::select(natural_disconnect, signal_disconnect).await {
                    embassy_futures::select::Either::First(_) => {
                        info!("Detected disconnect")
                    }
                    embassy_futures::select::Either::Second(y) => match y {
                        true => {
                            if let Some(ref mut x) = current_candidate {
                                x.connect_success = Some(true);
                            } else {
                                // there is no candidate, but clearly we're connected to something
                                // https://github.com/esp-rs/esp-hal/issues/4401
                            };
                        }
                        false => {
                            if let Some(ref mut x) = current_candidate {
                                x.connect_success = Some(false);
                            }
                            info!("Detected network instability");
                        }
                    },
                }

                Timer::after(Duration::from_millis(5000)).await;
            }
            // wifi::WifiStaState::Started => todo!(),
            // wifi::WifiStaState::Disconnected => todo!(),
            // wifi::WifiStaState::Stopped => todo!(),
            // wifi::WifiStaState::Invalid => todo!(),
            _ => {}
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(default_config.clone());

            controller.set_config(&client_config).unwrap();

            info!("Starting wifi");
            if let Err(x) = controller.start_async().await {
                info!("Error = {:?}", x);
            };
            info!("Started wifi");

            // scan nearby
            // todo: do this at a regular cadence/ or when there are no candidates
            let best_wgs = scan_and_score_wgs(&mut controller).await;

            // pick next candidate
            current_candidate = pick_next_candidate(best_wgs.first(), current_candidate.as_ref());
            info!("Candidate: {}", current_candidate);
            if let Some(best) = &current_candidate {
                ModeConfig::Client(get_client_config_from_candidate(&best));
                controller.set_config(&client_config).unwrap();
            }

            info!("About to connect ...");

            match controller.connect_async().await {
                Ok(_) => {
                    info!("Wifi Connected!");
                }
                Err(err) => {
                    info!("Failed to connect to wifi {:?}", err);
                    if let Some(ref mut x) = current_candidate {
                        // normally we would reserve connect success for an internet connection
                        x.connect_success = Some(false);
                    }
                    Timer::after(Duration::from_millis(5000)).await;
                }
            }
        }
    }
}

fn pick_next_candidate(
    scanned: Option<&WifiConfig>,
    current_candidate: Option<&WifiConfig>,
) -> Option<WifiConfig> {
    let current = match (scanned, current_candidate) {
        (None, None) => {
            // there are no valid candidates :-| ,
            None
        }
        (None, Some(x)) => {
            // the persisted config was not found in the scan
            Some(x.clone())
        }
        (Some(x), None) => {
            // there was no persisted config, a new winner emerges
            Some(x.clone())
        }
        (Some(scanned_best), Some(previous_best)) => {
            // there is candidate, and a persisted config
            if scanned_best == previous_best {
                // lo, it's the same config
                return current_candidate.cloned();
            }
            // scanned is better, we should use that
            if scanned_best > previous_best {
                Some(scanned_best.clone())
            }
            // persisted is better
            else {
                Some(previous_best.clone())
            }
        }
    };

    current
}
// async fn scan_loop( controller: &mut WifiController<'static>, current_candidate:    Option<WifiConfig>)
// {

//     let best_wgs = scan_and_score_wgs(&mut controller).await;

//     let best = best_wgs.first();
//     // we are required to connect to the previous WG, although theoretically a better one could
//     // have been found in the scan
//     // check if current persisted config  is best
//     if let Some(p) = &current_candidate
//         && best_wgs.contains(&p)
//     {
//         info!("Persisted Wifi found, using that...");
//         // already set
//     } else if let Some(best) = best_wgs.first() {
//         info!("Persisted Wifi not found, using the best WG in scan...");
//         if (best > current_candidate)
//         current_candidate = Some(best.clone());
//         let conf = get_client_config_from_ssid(&best);
//         let client_config = ModeConfig::Client(current_config.clone());
//         controller.set_config(&client_config).unwrap();
//         STORE_WIFI.signal(best.clone());
//     }

// }

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
