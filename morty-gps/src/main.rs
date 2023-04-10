use esp_idf_hal::adc;
use esp_idf_hal::adc::ADC1;
use esp_idf_hal::delay::BLOCK;
use esp_idf_hal::gpio;
use esp_idf_hal::gpio::ADCPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::prelude::*;
use esp_idf_hal::uart;
use esp_idf_hal::uart::Uart;
use esp_idf_svc::espnow::EspNow;
use esp_idf_svc::espnow::SendStatus;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::*;
use esp_idf_sys as _;
use esp_idf_sys::esp_deep_sleep_start;
use esp_idf_sys::esp_sleep_enable_timer_wakeup;
use lazy_static::lazy_static;
use log::*;
use morty_rs::comm::{broadcast_msg, esp_now_init};
use morty_rs::led::colors;
use morty_rs::led::Led;
use morty_rs::messages::*;
use morty_rs::utils::set_thread_spawn_configuration;
use morty_rs::utils::LastUpdate;
use morty_rs::GPS_UPDATE_INTERVAL_SECONDS;
use nmea0183::ParseResult;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

const LED_BRIGHTNESS: u8 = 10;

lazy_static! {
    static ref CHARGING: AtomicBool = AtomicBool::new(false);
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::log::EspLogger::initialize_default();
    let sysloop = EspSystemEventLoop::take()?;

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // Configure the LED
    let mut led = Led::new();
    led.start(pins.gpio18.into(), pins.gpio17.into())?;
    led.set_color(colors::BLUE, LED_BRIGHTNESS)?;

    // Configure Wifi for use with ESP-NOW
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi_driver = Box::new(EspWifi::new(peripherals.modem, sysloop, Some(nvs))?);
    wifi_driver.start()?;

    // Create a thread that reads the UART and transforms this into a protobuf to broadcast
    set_thread_spawn_configuration("uart-thread", 8196, 15, None)?;

    let uart_thread = std::thread::Builder::new()
        .stack_size(8196)
        .spawn(move || {
            uart_task(
                peripherals.uart1,
                pins.gpio0.into(),
                pins.gpio1.into(),
                pins.gpio33.into(),
                pins.gpio10,
                peripherals.adc1,
                led,
            )
            .unwrap();
        })?;

    uart_thread.join().unwrap();
    Ok(())
}

fn uart_task(
    uart: impl Peripheral<P = impl Uart> + 'static,
    tx: gpio::AnyOutputPin,
    rx: gpio::AnyInputPin,
    vbus_sense_pin: gpio::AnyInputPin,
    vbat_sense_pin: impl gpio::ADCPin<Adc = ADC1>,
    adc_peripheral: impl Peripheral<P = impl adc::Adc> + 'static,
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

    let vbus_sense = gpio::PinDriver::input(vbus_sense_pin)?;
    let mut vbat_driver =
        adc::AdcChannelDriver::<_, adc::Atten11dB<adc::ADC1>>::new(vbat_sense_pin)?;

    let mut adc1 = adc::AdcDriver::new(
        adc_peripheral,
        &adc::config::Config::new().calibration(true),
    )?;

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

                        let msg = GpsMsg {
                            latitude: gga.latitude.as_f64(),
                            longitude: gga.longitude.as_f64(),
                            satellites: gga.sat_in_use as i32,
                            fix_quality: gga.gps_quality as i32,
                            hdop: gga.hdop,
                            utc: gga.time.hours as i32 * 3600
                                + gga.time.minutes as i32 * 60
                                + gga.time.seconds as i32,
                            uid: Uuid::new_v4().to_string()[0..6].to_string(),
                            ..Default::default()
                        };

                        handle_message(
                            Some(msg),
                            &esp_now,
                            &vbus_sense,
                            &mut vbat_driver,
                            &mut adc1,
                            &mut led,
                            &mut last_update,
                        )?;
                    }
                    Ok(ParseResult::GGA(None)) => {
                        led.set_color(colors::RED, LED_BRIGHTNESS)?;

                        handle_message(
                            None,
                            &esp_now,
                            &vbus_sense,
                            &mut vbat_driver,
                            &mut adc1,
                            &mut led,
                            &mut last_update,
                        )?;
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
}

fn handle_message<T: gpio::ADCPin>(
    gps_message: Option<GpsMsg>,
    esp_now: &EspNow,
    vbus_sense: &gpio::PinDriver<<&mut gpio::AnyInputPin as Peripheral>::P, gpio::Input>,
    vbat_driver: &mut adc::AdcChannelDriver<T, adc::Atten11dB<adc::ADC1>>,
    adc: &mut adc::AdcDriver<impl adc::Adc>,
    led: &mut Led,
    last_update: &mut LastUpdate,
) -> Result<(), anyhow::Error>
where
    adc::Atten11dB<ADC1>: adc::Attenuation<<T as ADCPin>::Adc>,
{
    if last_update.should_update(Duration::from_secs(10)) {
        let (charging, battery_voltage) = check_power(vbus_sense, vbat_driver, adc)?;
        CHARGING.store(charging, Ordering::SeqCst);

        let blink_color = match &gps_message {
            Some(_) => colors::PURPLE,
            None => colors::RED,
        };

        let msg = match gps_message {
            Some(mut m) => {
                m.charging = charging;
                m.battery_voltage = battery_voltage;
                morty_message::Msg::Gps(m)
            }
            None => {
                let m = GpsMsg {
                    uid: Uuid::new_v4().to_string()[0..6].to_string(),
                    charging,
                    battery_voltage,
                    ..Default::default()
                };
                morty_message::Msg::Gps(m)
            }
        };

        led.blink_color(blink_color, LED_BRIGHTNESS, Duration::from_millis(300), 2)?;

        broadcast_msg(&msg, esp_now)?;
    }
    Ok(())
}

fn check_power<T: gpio::ADCPin>(
    vbus_sense: &gpio::PinDriver<<&mut gpio::AnyInputPin as Peripheral>::P, gpio::Input>,
    vbat_driver: &mut adc::AdcChannelDriver<T, adc::Atten11dB<adc::ADC1>>,
    adc: &mut adc::AdcDriver<impl adc::Adc>,
) -> Result<(bool, f32), anyhow::Error>
where
    adc::Atten11dB<ADC1>: adc::Attenuation<<T as ADCPin>::Adc>,
{
    // check if the device is powered by USB or battery

    let charging = vbus_sense.is_high();
    let voltage = adc.read(vbat_driver)?;
    Ok((charging, voltage as f32 / 262.0))
}

fn esp_now_send_cb(_dst: &[u8], status: SendStatus) {
    let charging = CHARGING.load(Ordering::SeqCst);
    if charging {
        return;
    }

    match status {
        SendStatus::SUCCESS => {
            info!("Going to sleep..");
            let us = Duration::from_secs(GPS_UPDATE_INTERVAL_SECONDS);
            unsafe {
                esp_sleep_enable_timer_wakeup(us.as_micros() as u64);
                esp_deep_sleep_start();
            }
        }
        SendStatus::FAIL => {}
    }
}
