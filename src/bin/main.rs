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
use wifi_scan_demo::{PASSWORD, PASSWORD2, SSID, SSID2, WifiConfig, scan_and_score_wgs};
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

// pub static a: Mutex<CriticalSectionRawMutex,RefCell<[WifiConfig;10]>> = Mutex::new(RefCell::new([WifiConfig::new();10]));

// next candidate wifi

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
    // LOAD_WIFI.signal(Some(WifiConfig {
    //     bssid: [152, 72, 39, 34, 83, 255],
    //     ssid: "It_hurts_when_IP_ext".to_string(),
    //     signal_strength: -31,
    // }));
    // let wifi_shared = &*mk_static!(RefCell::<WifiController>, RefCell::new(_wifi_controller));
    spawner.spawn(connection(_wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    // todo: consider moving into separate task
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get ip addr");

    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {:#}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_secs(1)).await;
        info!("Hello world!");
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let remote_endpoint = (Ipv4Addr::new(142, 250, 185, 115), 80);

        info!("Connecting...");

        let r = socket.connect(remote_endpoint).await;

        if let Err(e) = r {
            info!("connect error: {:?}", e);
            continue; // try again
        }

        info!("Socket connected");

        let mut buf = [0; 1024];

        loop {
            use embedded_io_async::Write;
            let r = Write::write_all(
                &mut socket,
                b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n",
            )
            .await;
            if let Err(e) = r {
                info!("Write Error {:?}", e);
                break;
            }

            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    info!("eof");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    info!("Read err: {:?}", e);
                    break;
                }
            };

            // if let Ok(num_bytes) = socket.read(&mut buf).await {
            info!("{}", core::str::from_utf8(&buf[..n]).unwrap());
            // break;
            // }
            // let r = socket
            //     .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
            //     .await;
        }
        Timer::after(Duration::from_millis(3000)).await;
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

fn get_client_config_from_ssid(wifi: &WifiConfig) -> ClientConfig {
    if wifi.ssid == SSID {
        ClientConfig::default()
            .with_ssid(SSID.into())
            .with_bssid(wifi.bssid)
            .with_password(PASSWORD.into())
    } else {
        ClientConfig::default()
            .with_ssid(SSID.into())
            .with_bssid(wifi.bssid)
            .with_password(PASSWORD2.into())
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) -> ! {
    info!("Start connection task");
    info!("Device Capabilities: {:?}", controller.capabilities());
    let default_config: ClientConfig = ClientConfig::default()
        .with_ssid(SSID.into())
        .with_password(PASSWORD.into());

    let mut current_config = default_config.clone();
    // persistence will load the previous connection from flash, if any
    let persisted_config = LOAD_WIFI.wait().await;

    let mut index = 0;
    use alloc::vec;
    let mut connection_queue: Vec<WifiConfig> = vec![];
    loop {
        match esp_radio::wifi::sta_state() {
            wifi::WifiStaState::Connected => {
                // todo: wait for disconnect event or check gateway status again
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await;
            }
            // wifi::WifiStaState::Started => todo!(),
            // wifi::WifiStaState::Disconnected => todo!(),
            // wifi::WifiStaState::Stopped => todo!(),
            // wifi::WifiStaState::Invalid => todo!(),
            _ => {}
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(current_config.clone());
            controller.set_config(&client_config).unwrap();

            info!("Starting wifi");
            if let Err(x) = controller.start_async().await {
                info!("Error = {:?}", x);
            };
            info!("Started wifi");

            let best_gws = scan_and_score_wgs(&mut controller).await;

            // check if current persisted config  is best
            if let Some(p) = &persisted_config
                && best_gws.contains(&p)
            {
                info!("Persisted Wifi found, using that...");
                // already set
            } else if let Some(best) = best_gws.first() {
                info!("Persisted Wifi not found, using the best WG in scan...");
                current_config = get_client_config_from_ssid(&best);
                let client_config = ModeConfig::Client(current_config.clone());
                controller.set_config(&client_config).unwrap();
                STORE_WIFI.signal(best.clone());
            }
        }

        // choose the next one in the connection queu

        info!("About to connect ...");

        match controller.connect_async().await {
            Ok(_) => {
                info!("Wifi Connected!");
            }
            Err(err) => {
                info!("Failed to connect to wifi {:?}", err);
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
