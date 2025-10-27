use anyhow::Error;
use defmt::info;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::partitions::{self, FlashRegion};
use esp_hal::peripherals;
use esp_storage::FlashStorage;

use crate::WifiConfig;

pub static LOAD_WIFI: Signal<CriticalSectionRawMutex, Option<WifiConfig>> = Signal::new();
pub static STORE_WIFI: Signal<CriticalSectionRawMutex, WifiConfig> = Signal::new();
const offset: u32 = 32;
#[embassy_executor::task]
pub async fn persistence(flash: peripherals::FLASH<'static>) -> ! {
    info!("Start persistence task");
    let mut flash = FlashStorage::new(flash);
    info!("Flash size = {}", flash.capacity());

    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];

    // read partitions
    let pt = partitions::read_partition_table(&mut flash, &mut pt_mem).unwrap();

    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .unwrap()
        .unwrap();
    let mut nvs_partition: FlashRegion<'_, FlashStorage<'_>> = nvs.as_embedded_storage(&mut flash);
    info!("NVS partition size = {}", nvs_partition.capacity());

    let conf = load_previous_wifi(&mut nvs_partition).await.ok();

    // notify connection thread
    LOAD_WIFI.signal(conf);
    let mut bytes = [0u8; 44];
    loop {
        info!("Waiting for new persistence");
        let conf: WifiConfig = STORE_WIFI.wait().await;
        info!("Persisting current best WG {:?}", conf);
        // erase first?
        const WIFI_CONFIG_ADDR: u32 = 0x0000;
        const WIFI_CONFIG_SECTOR_SIZE: u32 = 4096;
        let sector_start = offset - (offset % WIFI_CONFIG_SECTOR_SIZE);
        let sector_end = sector_start + WIFI_CONFIG_SECTOR_SIZE;
        nvs_partition.erase(sector_start, sector_end).unwrap();
        match postcard::to_slice::<WifiConfig>(&conf, &mut bytes) {
            Ok(x) => match nvs_partition.write(offset, &x) {
                Ok(_) => info!("Write success {:02x}", x),
                Err(y) => info!("Write error: {}", y),
            },
            Err(y) => info!("Error : {:?}", y),
        }
        Timer::after(Duration::from_millis(5000)).await;
        // listen for changes to wifi to persist
    }
}
// load the wifi
pub async fn load_previous_wifi<'a>(
    nvs_partition: &mut FlashRegion<'_, FlashStorage<'_>>,
) -> Result<WifiConfig, anyhow::Error> {
    let mut bytes = [0u8; 44];
    match nvs_partition.read(offset, &mut bytes) {
        Ok(_) => info!("Read bytes {:02x}", &bytes),
        Err(x) => info!("Errror = {:?}", x),
    }

    match postcard::from_bytes::<WifiConfig>(&bytes[..]) {
        Ok(x) => {
            info!("Config: {:?} ", x);
            return Ok(x);
        }
        Err(e) => {
            info!("Error {:?}", e);
            return Err(e.into());
        }
    }

    // starting wifi_config
}
