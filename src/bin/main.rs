#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::net::Ipv4Addr;
use core::result;

use defmt::info;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::{clock::CpuClock, rng::Rng};
use esp_radio::wifi::{ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent};
use esp_radio::{
    Controller,
    wifi::{self, ClientConfig},
};
use esp_rtos::embassy;
use ieee80211::{match_frames, mgmt_frame::BeaconFrame};
use {esp_backtrace as _, esp_println as _};

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

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const SSID2: &str = env!("SSID2");
const PASSWORD2: &str = env!("PASSWORD2");

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

    spawner.spawn(connection(_wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();
    // TODO: Spawn some tasks
    let _ = spawner;

    // todo: consider moving into separate task
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

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
            Write::write_all(
                &mut socket,
                b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n",
            )
            .await;
            // let r = socket
            //     .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
            //     .await;
            info!("{}", core::str::from_utf8(&buf[..]).unwrap());
        }
        Timer::after(Duration::from_millis(3000)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0-rc.1/examples/src/bin
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) -> ! {
    info!("Start connection task");
    info!("Device Capabilities: {:?}", controller.capabilities());

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
            // todo: switch config based on WG signal strength
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Started wifi");

            info!("scan");

            let scan_conf = ScanConfig::default().with_max(10);
            let result = controller.scan_with_config_async(scan_conf).await.unwrap();

            for ap in result {
                // todo: sort by signal strength
                info!("{:?}", ap);
            }
        }

        info!("About to connect ...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi Connected!"),
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
