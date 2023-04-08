use anyhow::bail;
use base64::engine::general_purpose;
use base64::Engine;
use embedded_svc::wifi::ClientConfiguration;
use embedded_svc::wifi::Configuration;
use esp_idf_hal::cpu::Core;
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
use log::*;
use morty_rs::comm::broadcast_data;
use morty_rs::comm::broadcast_msg;
use morty_rs::comm::decode_msg;
use morty_rs::comm::encode_msg;
use morty_rs::comm::esp_now_init;
use morty_rs::comm::mac_to_string;
use morty_rs::led::colors;
use morty_rs::led::Led;
use morty_rs::messages::*;
use morty_rs::utils::set_thread_spawn_configuration;
use morty_rs::BEACON_PRESENT_INTERVAL_SECONDS;
use std::net::Ipv4Addr;
use std::sync::mpsc::sync_channel;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

const SSID: &str = "IoT";
const PASS: &str = "EddieVedder7";

const LED_BRIGHTNESS: u8 = 10;

struct RecvData {
    src: Vec<u8>,
    data: Vec<u8>,
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();

    let sysloop = EspSystemEventLoop::take()?;
    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;
    let nvs = EspDefaultNvsPartition::take()?;

    // Configure the LED
    let mut led = Led::new();
    led.start(pins.gpio18.into(), pins.gpio17.into())?;
    led.set_color(colors::DARK_ORANGE, LED_BRIGHTNESS)?;

    let mut wifi = Box::new(EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?);
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
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

    led.set_color(colors::ORANGE, LED_BRIGHTNESS)?;

    // Update system time
    update_sntp()?;

    led.set_color(colors::GREEN, LED_BRIGHTNESS)?;

    // Disconnect from wifi and setup for ESP-NOW
    wifi.disconnect()?;
    wifi.stop()?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ..Default::default()
    }))?;
    wifi.start()?;

    // Channel for sending data to the recv thread
    let (recv_data_sender, recv_data_receiver) = sync_channel::<RecvData>(2);

    // Callback function for receiving data. This is executed on the main thread, so we keep this
    // as short as possible. We send the data to the recv thread via a channel.
    let esp_now_recv_cb = move |src: &[u8], data: &[u8]| {
        info!("Data recv from {}, len {}", mac_to_string(src), data.len());
        let recv_data = RecvData {
            src: src.to_vec(),
            data: data.to_vec(),
        };
        recv_data_sender.send(recv_data).unwrap();
    };

    // Initialize ESP-NOW and register the callback
    let esp_now = Arc::new(esp_now_init());
    esp_now.register_recv_cb(esp_now_recv_cb).unwrap();

    let beacon_espnow = esp_now.clone();
    // Spawn the beacon present thread on core 0
    set_thread_spawn_configuration("beacon-thread", 4196, 15, None)?;
    let beacon_thread = std::thread::Builder::new()
        .stack_size(4196)
        .spawn(move || loop {
            let msg = morty_message::Msg::BeaconPresent(BeaconPresentMsg {
                timestamp: EspSystemTime.now().as_secs() as i64,
            });
            broadcast_msg(&msg, &beacon_espnow).unwrap();
            std::thread::sleep(Duration::from_secs(BEACON_PRESENT_INTERVAL_SECONDS));
        })?;

    // Spawn the recv thread on core 1
    set_thread_spawn_configuration("recv-thread", 8196, 15, Some(Core::Core1))?;
    let recv_thread = std::thread::Builder::new()
        .stack_size(8196)
        .spawn(move || {
            recv_data_task(
                peripherals.uart1,
                pins.gpio1.into(),
                pins.gpio0.into(),
                &esp_now,
                recv_data_receiver,
                &mut led,
            )
            .unwrap();
        })?;

    beacon_thread.join().unwrap();
    recv_thread.join().unwrap();
    Ok(())
}

fn recv_data_task(
    uart: impl Peripheral<P = impl Uart> + 'static,
    tx: gpio::AnyOutputPin,
    rx: gpio::AnyInputPin,
    esp_now: &esp_idf_svc::espnow::EspNow,
    recv_data_receiver: Receiver<RecvData>,
    led: &mut Led,
) -> Result<(), anyhow::Error> {
    let uart = uart_init(uart, tx, rx)?;

    loop {
        let recv_data = recv_data_receiver.recv().unwrap();
        let src = mac_to_string(recv_data.src.as_slice());
        match decode_msg(&recv_data.data) {
            Ok(Some(morty_message::Msg::Gps(gps))) => {
                info!("GPS from {src}: {:?}", gps);
                let now = EspSystemTime.now().as_secs() as i64;

                let relay_msg = RelayMsg {
                    timestamp: now,
                    src,
                    msg: Some(morty_rs::messages::relay_msg::Msg::Gps(gps)),
                };

                let data = encode_msg(&morty_message::Msg::Relay(relay_msg));

                // Broadcast over ESP-NOW
                broadcast_data(&data, esp_now)?;

                // Send over UART
                uart_write(&uart, &data)?;
                led.blink_color(
                    colors::PURPLE,
                    LED_BRIGHTNESS,
                    Duration::from_millis(300),
                    2,
                )?;
            }
            Ok(Some(morty_message::Msg::Relay(relay))) => {
                info!("Relay from {src}: {:?}", relay);
                let data = encode_msg(&morty_message::Msg::Relay(relay));
                uart_write(&uart, &data)?;
                led.blink_color(colors::BLUE, LED_BRIGHTNESS, Duration::from_millis(100), 2)?;
            }
            Ok(Some(morty_message::Msg::BeaconPresent(beacon))) => {
                info!("Beacon from {src}: {:?}", beacon);
            }
            Err(e) => {
                error!("Error decoding message: {e}");
            }
            Ok(None) => {
                warn!("No message received")
            }
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

fn uart_init(
    uart: impl Peripheral<P = impl Uart> + 'static,
    tx: gpio::AnyOutputPin,
    rx: gpio::AnyInputPin,
) -> Result<UartDriver<'static>, anyhow::Error> {
    let config = uart::config::Config::default().baudrate(Hertz(115200));
    let uart_driver = uart::UartDriver::new(
        uart,
        tx,
        rx,
        Option::<gpio::Gpio0>::None,
        Option::<gpio::Gpio0>::None,
        &config,
    )?;

    Ok(uart_driver)
}

fn uart_write(uart: &UartDriver, data: &[u8]) -> Result<(), anyhow::Error> {
    const UART_HEADER: &str = "MORTYGPS";
    let b64_encoded = general_purpose::STANDARD.encode(data);
    let bytes = b64_encoded.as_bytes();
    uart.write(UART_HEADER.as_bytes())?;
    uart.write(bytes)?;
    uart.write(b"\n")?;
    info!("Wrote {} bytes over UART", bytes.len());
    Ok(())
}
