# wifi-scan-demo

Small ESP32 demo that scans for known Wi‑Fi gateways (WGs), ranks candidates, attempts connections, and persists the best gateway to flash.


## Prerequisites
- Follow the [guide] to setup the build toolchain and pipeline(https://docs.espressif.com/projects/rust/book/getting-started/toolchain.html)

- Ensure your host has the appropriate USB/serial permissions for flashing.

.env defaults are set in [.cargo/config.toml](.cargo/config.toml) (SSID, PASSWORD, etc.). Edit them before building if needed.

## Build and flash
The local Cargo config includes a runner that calls `espflash` with defmt support. From the repo root:

- Build (release):
```sh
cargo run --release
```

## Working Principle

1. Startup (see src/bin/main.rs):

- Initialization of peripherals, heap, and networking stack.
- Spawns the persistence task wifi_scan_demo::persistence, the Wi‑Fi manager task wifi_mgr, the best‑connection scanner best_connection_task, and the network task (net_task).

2. Persistence (see src/persistence.rs):

- On start, persistence reads the NVS partition and attempts to load the previously persisted `WifiConfig` (signals that value through LOAD_WIFI).
- When the connection logic finds a new best gateway, it signals STORE_WIFI and persistence serializes the chosen `wifi_scan_demo::WifiConfig` into flash (uses postcard).

3. Scanning & Ranking (see src/lib.rs):

- wifi_scan_demo::scan_and_score_wgs uses the radio controller to scan nearby APs and filters for the baked‑in SSIDs (wifi_scan_demo::KNOWN_CREDS).
- It maps scan results into `WifiConfig` records and sorts them using the Ord/ranking logic on `WifiConfig` (connected-success state + RSSI).

4. Connection manager (see src/bin/main.rs):

- `wifi_mgr` sets up the client configuration and maintains the Wi‑Fi station state.
- When disconnected it will pick the top candidate from CANDIDATES and attempt to connect.
- `best_connection_task` monitors scans and persistence to decide when to re‑scan and when to update persisted best gateway.

5. Runtime signals & shared state

- Control is coordinated via Embassy signals and a mutex:
- `SCAN_CMD` / `SCAN_COMPLETE` — trigger and acknowledge scans.
- `CANDIDATES` — shared candidate list (embassy mutex).
- `WG_CONNECT_STATUS` — connection health signal (not used ATM)
- `DISCONNECT_DETECTED` — used to adapt scan frequency after disconnects.
- The network stack runs in `net_task` and the main loop tries TCP connectivity to 
`1.1.1.1:80` to validate internet connectivity.
