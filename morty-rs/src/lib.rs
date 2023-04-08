pub mod comm;
pub mod led;
pub mod utils;
pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/morty.messages.rs"));
}

pub const GPS_UPDATE_INTERVAL_SECONDS: u64 = 10;
pub const BEACON_PRESENT_INTERVAL_SECONDS: u64 = 10;
