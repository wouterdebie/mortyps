use esp_idf_hal::delay::BLOCK;
use esp_idf_hal::gpio;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::prelude::*;
use esp_idf_hal::uart;
use esp_idf_hal::uart::Uart;
use esp_idf_svc::espnow::SendStatus;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::*;
use esp_idf_sys as _;
use esp_idf_sys::esp_deep_sleep_start;
use esp_idf_sys::esp_sleep_enable_timer_wakeup;
use log::*;
use morty_rs::comm::{broadcast_msg, esp_now_init, mac_to_string};
use morty_rs::led::colors;
use morty_rs::led::Led;
use morty_rs::messages::*;
use morty_rs::utils::set_thread_spawn_configuration;
use morty_rs::utils::LastUpdate;
use morty_rs::GPS_UPDATE_INTERVAL_SECONDS;
use nmea0183::ParseResult;
use std::time::Duration;
use uuid::Uuid; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

const STAY_ALIVE: bool = true;
const LED_BRIGHTNESS: u8 = 10;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();
    let sysloop = EspSystemEventLoop::take()?;

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    let nvs = EspDefaultNvsPartition::take()?;

    let mut led = Led::new();
    led.start(pins.gpio18.into(), pins.gpio17.into())?;
    led.set_color(colors::BLUE, LED_BRIGHTNESS)?;

    let mut wifi_driver = Box::new(EspWifi::new(peripherals.modem, sysloop, Some(nvs))?);
    wifi_driver.start()?;

    // Create a thread that reads the UART and transforms this into a protobuf to broadcast
    set_thread_spawn_configuration("uart-thread", 8196, 15, None)?;

    let uart_thread = std::thread::Builder::new()
        .stack_size(8196)
        .spawn(move || {
            uart_task(peripherals.uart1, pins.gpio0.into(), pins.gpio1.into(), led).unwrap();
        })?;

    uart_thread.join().unwrap();
    Ok(())
}

fn uart_task(
    uart: impl Peripheral<P = impl Uart> + 'static,
    tx: gpio::AnyOutputPin,
    rx: gpio::AnyInputPin,
    mut led: Led,
) -> Result<(), anyhow::Error> {
    let config = uart::config::Config::default().baudrate(Hertz(9600));

    let uart_driver = uart::UartDriver::new(
        uart,
        tx,
        rx,
        Option::<gpio::Gpio0>::None,
        Option::<gpio::Gpio0>::None,
        &config,
    )?;

    uart_driver.flush_read()?;

    let mut nmea_parser = nmea0183::Parser::new();

    let esp_now = esp_now_init();
    esp_now.register_send_cb(esp_now_send_cb)?;

    let mut buf = [0u8; 1];

    // Keep track of last updated time
    let mut last_update = LastUpdate::new();

    loop {
        if uart_driver.read(&mut buf, BLOCK).is_ok() {
            if let Some(result) = nmea_parser.parse_from_byte(buf[0]) {
                match result {
                    Ok(ParseResult::GGA(Some(gga))) => {
                        led.set_color(colors::GREEN, LED_BRIGHTNESS)?;

                        // Only update ever so often.
                        if last_update
                            .should_update(Duration::from_secs(GPS_UPDATE_INTERVAL_SECONDS))
                        {
                            let uid = Uuid::new_v4().to_string()[0..6].to_string();

                            let msg = morty_message::Msg::Gps(GpsMsg {
                                latitude: gga.latitude.as_f64(),
                                longitude: gga.longitude.as_f64(),
                                satellites: gga.sat_in_use as i32,
                                fix_quality: gga.gps_quality as i32,
                                hdop: gga.hdop,
                                utc: gga.time.hours as i32 * 3600
                                    + gga.time.minutes as i32 * 60
                                    + gga.time.seconds as i32,
                                uid,
                            });

                            broadcast_msg(&msg, &esp_now)?;
                            led.blink_color(
                                colors::PURPLE,
                                LED_BRIGHTNESS,
                                Duration::from_millis(300),
                                2,
                            )?;
                        }
                    }
                    Ok(ParseResult::GGA(None)) => {
                        led.set_color(colors::RED, LED_BRIGHTNESS)?;
                        let uid = Uuid::new_v4().to_string()[0..6].to_string();
                        let gps_message = GpsMsg {
                            uid,
                            ..Default::default()
                        };

                        let msg = morty_message::Msg::Gps(gps_message);
                        if last_update.should_update(Duration::from_secs(10)) {
                            broadcast_msg(&msg, &esp_now)?;
                            led.blink_color(
                                colors::PURPLE,
                                LED_BRIGHTNESS,
                                Duration::from_millis(300),
                                2,
                            )?;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
}

fn esp_now_send_cb(dst: &[u8], status: SendStatus) {
    if STAY_ALIVE {
        return;
    }

    match status {
        SendStatus::SUCCESS => {
            info!(
                "Sent data to {}. Going to sleep for {GPS_UPDATE_INTERVAL_SECONDS} seconds",
                mac_to_string(dst)
            );
            let us = Duration::from_secs(GPS_UPDATE_INTERVAL_SECONDS);
            unsafe {
                esp_sleep_enable_timer_wakeup(us.as_micros() as u64);
                esp_deep_sleep_start();
            }
        }
        _ => {
            error!("Failed to send data to {}", mac_to_string(dst));
        }
    }
}
