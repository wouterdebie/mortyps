use esp_idf_hal::uart::UartDriver;
use esp_idf_hal::{delay::BLOCK, task::thread::ThreadSpawnConfiguration};
use esp_idf_svc::timer::EspTimerService;
use esp_idf_sys::EspError;
use hexdump::hexdump_iter;
use log::*;
use std::{io::Read, time::Duration};

pub struct LastUpdate {
    last_update: Duration,
    timer_service: EspTimerService<esp_idf_svc::timer::Task>,
}
impl Default for LastUpdate {
    fn default() -> Self {
        Self::new()
    }
}

impl LastUpdate {
    pub fn new() -> Self {
        Self {
            last_update: Duration::from_secs(0),
            timer_service: EspTimerService::new().unwrap(),
        }
    }

    pub fn should_update(&mut self, since: Duration) -> bool {
        let now = self.timer_service.now();
        if Duration::is_zero(&self.last_update) || now - self.last_update >= since {
            self.last_update = now;
            true
        } else {
            false
        }
    }
}

pub fn set_thread_spawn_configuration(
    name: &'static str,
    stack_size: usize,
    prio: u8,
    pin_to_core: Option<esp_idf_hal::cpu::Core>,
) -> Result<(), EspError> {
    ThreadSpawnConfiguration {
        name: Some(name.as_bytes()),
        stack_size,
        priority: prio,
        pin_to_core,
        ..Default::default()
    }
    .set()
}

pub fn log_hexdump(data: &[u8]) {
    let iter = hexdump_iter(data);
    for line in iter {
        info!("{}", line);
    }
}

pub fn tname() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string()
}

pub struct UartRead<'a> {
    uart: UartDriver<'a>,
}

impl<'a> UartRead<'a> {
    pub fn new(uart: UartDriver<'a>) -> Self {
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
