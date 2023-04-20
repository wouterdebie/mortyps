use anyhow::bail;
use base64::engine::general_purpose;
use base64::Engine;
use embedded_svc::wifi;
use esp_idf_hal::cpu::Core;
use esp_idf_hal::gpio;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::prelude::*;
use esp_idf_hal::uart;
use esp_idf_hal::uart::Uart;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::netif::EspNetif;
use esp_idf_svc::netif::EspNetifWait;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::SyncStatus;
use esp_idf_svc::systime::EspSystemTime;
use esp_idf_svc::wifi::*;
use esp_idf_sys as _;
use json::object;
use log::*;
use morty_rs::comm::decode_msg;
use morty_rs::led::colors;
use morty_rs::led::Led;
use morty_rs::messages::morty_message::Msg;
use morty_rs::utils::set_thread_spawn_configuration;
use morty_rs::utils::UartRead;
use std::collections::VecDeque;
use std::io::BufRead;
use std::io::BufReader;
use std::net::Ipv4Addr;
use std::time::Duration; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

const SSID: &str = "IoT";
const PASS: &str = "EddieVedder7";

const LED_BRIGHTNESS: u8 = 10;
const API_HOST: &str = "wouterdebie-personal.ue.r.appspot.com";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();

    let sysloop = EspSystemEventLoop::take()?;
    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;
    let nvs = EspDefaultNvsPartition::take()?;

    // Configure the LED
    let mut led = Led::new();
    led.start(pins.gpio18.into(), pins.gpio17.into())?;
    led.set_color(colors::BLUE, LED_BRIGHTNESS)?;

    // Configure the wifi
    let mut wifi = Box::new(EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?);
    wifi.set_configuration(&wifi::Configuration::Client(wifi::ClientConfiguration {
        ssid: SSID.into(),
        password: PASS.into(),
        ..Default::default()
    }))?;

    wifi.start()?;
    if !WifiWait::new(&sysloop)?
        .wait_with_timeout(Duration::from_secs(20), || wifi.is_started().unwrap())
    {
        bail!("Wifi did not start");
    }

    wifi.connect()?;

    if !EspNetifWait::new::<EspNetif>(wifi.sta_netif(), &sysloop)?.wait_with_timeout(
        Duration::from_secs(20),
        || {
            wifi.is_up().unwrap()
                && wifi.sta_netif().get_ip_info().unwrap().ip != Ipv4Addr::new(0, 0, 0, 0)
        },
    ) {
        bail!("Wifi did not connect or did not receive a DHCP lease");
    }
    led.set_color(colors::YELLOW, LED_BRIGHTNESS)?;

    // Update system time
    update_sntp()?;

    led.set_color(colors::GREEN, LED_BRIGHTNESS)?;

    // Spawn the recv thread on core 1
    set_thread_spawn_configuration("recv-thread\0", 8196, 15, Some(Core::Core1))?;
    let recv_thread = std::thread::Builder::new()
        .stack_size(8196)
        .spawn(move || {
            uart_task(peripherals.uart1, pins.gpio0.into(), pins.gpio2.into(), led).unwrap();
        })?;

    recv_thread.join().unwrap();
    Ok(())
}

//// Receive RelayMsgs from a beacon over UART and send them as JSON to a server in the cloud.
fn uart_task(
    uart: impl Peripheral<P = impl Uart> + 'static,
    tx: gpio::AnyOutputPin,
    rx: gpio::AnyInputPin,
    mut led: Led,
) -> Result<(), anyhow::Error> {
    info!("Starting UART task");
    let config = uart::config::Config::default().baudrate(Hertz(115200));

    let uart_driver = uart::UartDriver::new(
        uart,
        tx,
        rx,
        Option::<gpio::Gpio0>::None,
        Option::<gpio::Gpio0>::None,
        &config,
    )?;

    // Create a cache of the last 10 IDs we've seen, since we can have multiple messages with the
    // same id, because a message might have been relayed by multiple beacons.
    let mut cache = IdCache::new(10);

    uart_driver.flush_read()?;

    let mut reader = BufReader::new(UartRead::new(uart_driver));
    let mut buffer = String::new();

    loop {
        buffer.clear();
        reader.read_line(&mut buffer)?;
        if &buffer[0..8] != "MORTYGPS" {
            warn!("Received invalid message: {}", buffer);
        } else {
            // Decode Base64
            let bytes = general_purpose::STANDARD.decode(buffer[8..].trim());
            if bytes.is_err() {
                error!("Unable to decode: {}", buffer);
                continue;
            }

            // Decode protobuf
            let morty_msg = decode_msg(bytes.unwrap().as_slice());
            match morty_msg {
                Ok(Some(Msg::Relay(relay_msg))) => {
                    handle_relay_message(relay_msg, &mut cache, &mut led).unwrap();
                }
                Ok(msg) => {
                    warn!("Received unknown message: {:?}", msg);
                }
                Err(e) => {
                    error!("Error decoding message: {:?}", e);
                }
            };
        }
    }
}

// Handle the relay message
fn handle_relay_message(
    relay_message: morty_rs::messages::RelayMsg,
    cache: &mut IdCache,
    led: &mut Led,
) -> Result<(), anyhow::Error> {
    match relay_message.msg {
        Some(morty_rs::messages::relay_msg::Msg::Gps(gps)) => {
            info!("Received GPS: {:?}", gps);

            // Check if we have already seen the message by its UID
            if !cache.contains(&gps.uid) {
                let uri = format!(
                    "https://{API_HOST}/api/v1/source/{}/location",
                    relay_message.src
                );

                // Create a json object
                let json = object! {
                    "latitude": gps.latitude,
                    "longitude": gps.longitude,
                    "hdop": gps.hdop,
                    "timestamp": relay_message.timestamp,
                    "utc": gps.utc,
                    "fix_quality": gps.fix_quality,
                    "satellites": gps.satellites,
                    "uid" : gps.uid.to_string(),
                    "charging": gps.charging,
                    "battery_voltage": gps.battery_voltage,
                }
                .dump();

                let data = json.as_bytes();

                // Send stuff to the API server over HTTPS
                let mut client = embedded_svc::http::client::Client::wrap(
                    esp_idf_svc::http::client::EspHttpConnection::new(
                        &esp_idf_svc::http::client::Configuration {
                            crt_bundle_attach: Some(esp_idf_sys::esp_crt_bundle_attach),

                            ..Default::default()
                        },
                    )?,
                );

                let headers = [
                    ("Content-Type", "application/json"),
                    ("Content-Length", &format!("{}", data.len())),
                ];

                let mut request = client.post(&uri, &headers)?;
                request.connection().write(data)?;
                let mut response = request.submit()?;

                let mut body = [0_u8; 128];
                let read = embedded_svc::utils::io::try_read_full(&mut response, &mut body)
                    .map_err(|err| err.0)?;
                info!(
                    "Response: {}",
                    String::from_utf8_lossy(&body[..read]).into_owned().trim()
                );
                use embedded_svc::io::Read;
                // Complete the response
                while response.read(&mut body)? > 0 {}

                cache.add(&gps.uid);
                led.blink_color(
                    colors::PURPLE,
                    LED_BRIGHTNESS,
                    Duration::from_millis(300),
                    2,
                )?;
            } else {
                // Blink the LED when it's a duplicate message
                led.blink_color(
                    colors::ORANGE,
                    LED_BRIGHTNESS,
                    Duration::from_millis(300),
                    2,
                )?;
            }
        }
        _ => {
            warn!("Received unknown message: {:?}", relay_message);
        }
    }
    Ok(())
}

fn update_sntp() -> Result<(), anyhow::Error> {
    let sntp = esp_idf_svc::sntp::EspSntp::new_default()?;
    while sntp.get_sync_status() != SyncStatus::Completed {
        info!("Waiting for SNTP to sync");
        std::thread::sleep(Duration::from_secs(1));
    }
    let now = EspSystemTime.now();
    info!("Current time: {:?}", now);
    Ok(())
}

struct IdCache {
    data: VecDeque<String>,
    size: usize,
}

impl IdCache {
    pub fn new(size: usize) -> Self {
        Self {
            data: VecDeque::new(),
            size,
        }
    }

    fn add(&mut self, data: &str) {
        self.data.push_back(data.to_string());
        if self.data.len() > self.size {
            self.data.pop_front();
        }
    }

    fn contains(&self, data: &str) -> bool {
        self.data.contains(&data.to_string())
    }
}
