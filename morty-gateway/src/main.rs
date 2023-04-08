use anyhow::bail;
use base64::engine::general_purpose;
use base64::Engine;
use embedded_svc::wifi;
use esp_idf_hal::cpu::Core;
use esp_idf_hal::delay::BLOCK;
use esp_idf_hal::gpio;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::prelude::*;
use esp_idf_hal::uart;
use esp_idf_hal::uart::Uart;
use esp_idf_hal::uart::UartDriver;
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
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::net::Ipv4Addr;
use std::time::Duration; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

const SSID: &str = "IoT";
const PASS: &str = "EddieVedder7";

const LED_BRIGHTNESS: u8 = 10;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();
    if esp_idf_sys::CONFIG_MAIN_TASK_STACK_SIZE < 20000 {
        error!(
            "stack too small: {} bail!",
            esp_idf_sys::CONFIG_MAIN_TASK_STACK_SIZE
        );
        return Ok(());
    }

    let sysloop = EspSystemEventLoop::take()?;

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    let nvs = EspDefaultNvsPartition::take()?;

    let mut led = Led::new();
    led.start(pins.gpio18.into(), pins.gpio17.into())?;
    led.set_color(colors::BLUE, LED_BRIGHTNESS)?;

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
    set_thread_spawn_configuration("recv-thread", 8196, 15, Some(Core::Core1))?;
    let recv_thread = std::thread::Builder::new()
        .stack_size(8196)
        .spawn(move || {
            uart_task(peripherals.uart1, pins.gpio0.into(), pins.gpio2.into(), led).unwrap();
        })?;

    recv_thread.join().unwrap();
    Ok(())
}

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

    let mut cache = IdCache::new(10);

    uart_driver.flush_read()?;

    let mut reader = BufReader::new(UartRead::new(uart_driver));
    let mut buffer = String::new();
    loop {
        buffer.clear();
        reader.read_line(&mut buffer)?;
        info!("Received message: {}", buffer.trim());
        if &buffer[0..8] != "MORTYGPS" {
            warn!("Received invalid message: {}", buffer);
        } else {
            let bytes = general_purpose::STANDARD.decode(buffer[8..].trim());
            if bytes.is_err() {
                error!("Unable to decode: {}", buffer);
                continue;
            }

            let morty_msg = decode_msg(bytes.unwrap().as_slice());
            match morty_msg {
                Ok(Some(Msg::Relay(relay_msg))) => {
                    handle_relay_message(relay_msg, &mut cache).unwrap();
                    led.blink_color(colors::BLUE, LED_BRIGHTNESS, Duration::from_millis(500), 2)?;
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

fn handle_relay_message(
    relay_message: morty_rs::messages::RelayMsg,
    cache: &mut IdCache,
) -> Result<(), anyhow::Error> {
    match relay_message.msg {
        Some(morty_rs::messages::relay_msg::Msg::Gps(gps)) => {
            info!("Received GPS: {:?}", gps);
            if !cache.contains(&gps.uid) {
                let uri = format!(
                    "https://wouterdebie-personal.ue.r.appspot.com/api/v1/source/{}/location",
                    relay_message.src
                );

                let url = CString::new(uri.as_str()).unwrap();

                let config_post = esp_idf_sys::esp_http_client_config_t {
                    url: url.as_ptr() as _,
                    method: esp_idf_sys::esp_http_client_method_t_HTTP_METHOD_POST,
                    auth_type: esp_idf_sys::esp_http_client_auth_type_t_HTTP_AUTH_TYPE_NONE,
                    transport_type:
                        esp_idf_sys::esp_http_client_transport_t_HTTP_TRANSPORT_OVER_TCP,
                    event_handler: Some(handler),
                    ..Default::default()
                };

                let data = object! {
                    "latitude": gps.latitude,
                    "longitude": gps.longitude,
                    "hdop": gps.hdop,
                    "timestamp": relay_message.timestamp,
                    "utc": gps.utc,
                    "fix_quality": gps.fix_quality,
                    "satellites": gps.satellites,
                    "uid" : gps.uid.to_string(),
                }
                .dump();

                let c_data = CString::new(data.as_str()).unwrap();
                let c_header = CString::new("Content-Type").unwrap();
                let c_header_value = CString::new("application/json").unwrap();

                unsafe {
                    let client = esp_idf_sys::esp_http_client_init(&config_post);
                    esp_idf_sys::esp_http_client_set_timeout_ms(client, 10000);
                    esp_idf_sys::esp_http_client_set_post_field(
                        client,
                        c_data.as_ptr() as _,
                        data.len() as i32,
                    );
                    esp_idf_sys::esp_http_client_set_header(
                        client,
                        c_header.as_ptr() as _,
                        c_header_value.as_ptr() as _,
                    );
                    esp_idf_sys::esp_http_client_perform(client);
                    esp_idf_sys::esp_http_client_close(client);
                    esp_idf_sys::esp_http_client_cleanup(client);
                }

                cache.add(&gps.uid);
            }
        }
        _ => {
            warn!("Received unknown message: {:?}", relay_message);
        }
    }
    Ok(())
}

unsafe extern "C" fn handler(
    evt: *mut esp_idf_sys::esp_http_client_event,
) -> esp_idf_sys::esp_err_t {
    match *evt {
        esp_idf_sys::esp_http_client_event_t {
            event_id: esp_idf_sys::esp_http_client_event_id_t_HTTP_EVENT_ERROR,
            ..
        } => {
            error!("HTTP_EVENT_ERROR");
        }
        esp_idf_sys::esp_http_client_event_t {
            event_id: esp_idf_sys::esp_http_client_event_id_t_HTTP_EVENT_ON_DATA,
            data,
            data_len,
            ..
        } => {
            let data = unsafe { std::slice::from_raw_parts(data as *const u8, data_len as usize) };
            let data = std::str::from_utf8(data).unwrap();
            info!("HTTP_EVENT_ON_DATA: {:?}", data);
        }
        _ => {}
    }
    esp_idf_sys::ESP_OK as _
}

struct UartRead<'a> {
    uart: UartDriver<'a>,
}

impl<'a> UartRead<'a> {
    fn new(uart: UartDriver<'a>) -> Self {
        Self { uart }
    }
}

impl<'a> Read for UartRead<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut b: [u8; 1] = [0];

        match self.uart.read(&mut b, BLOCK) {
            Ok(size) => {
                buf[0] = b[0];
                Ok(size)
            }
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error reading from UART",
            )),
        }
    }
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
