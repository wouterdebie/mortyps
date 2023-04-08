use crate::utils::set_thread_spawn_configuration;
use esp_idf_hal::cpu::Core;
use esp_idf_hal::gpio;
use esp_idf_hal::gpio::Pin;
use esp_idf_hal::gpio::PinDriver;
pub use smart_leds::colors;
use smart_leds::SmartLedsWrite;
use smart_leds::RGB8;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

enum LedCommand {
    SetColor {
        color: RGB8,
        brightness: u8,
    },
    Blink {
        color: RGB8,
        brightness: u8,
        period: Duration,
        duty_cycle: u8,
        times: u8,
    },
}
pub struct Led {
    driver_handle: Option<thread::JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    cmd_tx: Option<std::sync::mpsc::Sender<LedCommand>>,
}

impl Default for Led {
    fn default() -> Self {
        Self::new()
    }
}

impl Led {
    pub fn new() -> Self {
        Self {
            driver_handle: None,
            alive: Arc::new(AtomicBool::new(false)),
            cmd_tx: None,
        }
    }

    pub fn start(
        &mut self,
        led_pin: gpio::AnyOutputPin,
        power_pin: gpio::AnyOutputPin,
    ) -> anyhow::Result<()> {
        self.alive.store(true, Ordering::SeqCst);
        let alive = self.alive.clone();

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<LedCommand>();
        self.cmd_tx = Some(cmd_tx);

        set_thread_spawn_configuration("led-htread", 4196, 15, Some(Core::Core1))?;
        self.driver_handle = Some(
            std::thread::Builder::new()
                .stack_size(4196)
                .spawn(move || {
                    // Set the power to high
                    let mut led = PinDriver::output(power_pin).unwrap();
                    led.set_high().unwrap();

                    let mut ws2812 = ws2812_esp32_rmt_driver::Ws2812Esp32Rmt::new(
                        0,
                        led_pin.pin().try_into().unwrap(),
                    )
                    .unwrap();

                    let mut current_color = colors::BLACK;

                    while alive.load(Ordering::SeqCst) {
                        match cmd_rx.recv().unwrap() {
                            LedCommand::SetColor { color, brightness } => {
                                current_color = apply_brightness(color, brightness);
                                ws2812
                                    .write(std::iter::repeat(current_color).take(1))
                                    .unwrap();
                            }
                            LedCommand::Blink {
                                color,
                                brightness,
                                period,
                                duty_cycle,
                                times,
                            } => {
                                let color = apply_brightness(color, brightness);

                                let pos_half = period * duty_cycle as u32 / 100;
                                let neg_half = period * (100 - duty_cycle) as u32 / 100;

                                for _ in 0..times {
                                    ws2812.write(std::iter::repeat(color).take(1)).unwrap();

                                    std::thread::sleep(pos_half);
                                    ws2812
                                        .write(std::iter::repeat(colors::BLACK).take(1))
                                        .unwrap();
                                    std::thread::sleep(neg_half);
                                }
                                ws2812
                                    .write(std::iter::repeat(current_color).take(1))
                                    .unwrap()
                            }
                        };
                    }
                })
                .unwrap(),
        );

        Ok(())
    }

    pub fn stop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.driver_handle
            .take()
            .expect("Called stop on non-running thread")
            .join()
            .expect("Could not join spawned thread");
    }

    pub fn set_color(&mut self, color: RGB8, brightness: u8) -> anyhow::Result<()> {
        match self.cmd_tx {
            Some(ref tx) => tx
                .send(LedCommand::SetColor { color, brightness })
                .map_err(anyhow::Error::msg),
            None => Err(anyhow::anyhow!("Led not started")),
        }
    }

    pub fn blink_color(
        &mut self,
        color: RGB8,
        brightness: u8,
        period: Duration,
        times: u8,
    ) -> anyhow::Result<()> {
        match self.cmd_tx {
            Some(ref tx) => tx
                .send(LedCommand::Blink {
                    color,
                    brightness,
                    period,
                    duty_cycle: 50,
                    times,
                })
                .map_err(anyhow::Error::msg),
            None => Err(anyhow::anyhow!("Led not started")),
        }
    }
}

fn apply_brightness(color: RGB8, brightness: u8) -> RGB8 {
    RGB8::new(
        (color.r as u16 * (brightness as u16 + 1) / 256) as u8,
        (color.g as u16 * (brightness as u16 + 1) / 256) as u8,
        (color.b as u16 * (brightness as u16 + 1) / 256) as u8,
    )
}
