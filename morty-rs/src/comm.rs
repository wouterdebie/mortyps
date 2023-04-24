use std::{net::Ipv4Addr, time::Duration};

use crate::messages::{morty_message, MortyMessage};
use anyhow::{anyhow, bail};
use crc8::Crc8;
use embedded_svc::wifi::ClientConfiguration;
use embedded_svc::wifi::Configuration;
use esp_idf_svc::{
    espnow::{EspNow, PeerInfo, BROADCAST},
    eventloop::EspSystemEventLoop,
    netif::{EspNetif, EspNetifWait},
    wifi::{EspWifi, WifiWait},
};
use log::*;
use prost::Message;

pub const ESP_NOW_CHANNEL: u8 = 1;

pub fn esp_now_init() -> EspNow {
    let esp_now = EspNow::take().unwrap();

    esp_now
        .add_peer(PeerInfo {
            peer_addr: BROADCAST,
            channel: ESP_NOW_CHANNEL,
            ifidx: 0,
            encrypt: false,
            ..Default::default()
        })
        .unwrap();
    esp_now
}

pub fn get_message_type(msg: &Option<morty_message::Msg>) -> u8 {
    match msg {
        Some(morty_message::Msg::BeaconPresent(_)) => 1,
        Some(morty_message::Msg::Gps(_)) => 2,
        Some(morty_message::Msg::Relay(_)) => 3,
        None => 0,
    }
}

pub fn broadcast_msg(msg: &morty_message::Msg, esp_now: &EspNow) -> Result<(), anyhow::Error> {
    info!("Broadcasting message: {:?}", msg);
    let data = encode_msg(msg);
    broadcast_data(&data, esp_now)
}

pub fn broadcast_data(data: &Vec<u8>, esp_now: &EspNow) -> Result<(), anyhow::Error> {
    esp_now.send(BROADCAST, data.as_slice())?;
    Ok(())
}

pub fn encode_msg(msg: &morty_message::Msg) -> Vec<u8> {
    let morty_message = MortyMessage {
        msg: Some(msg.clone()),
    };

    let msg_type = &[get_message_type(&morty_message.msg)];
    let vec = morty_message.encode_to_vec();
    let bytes = vec.as_slice();

    let mut crc8 = Crc8::create_msb(0x07);
    let crc = &[crc8.calc(bytes, bytes.len() as i32, 0)];

    [msg_type, crc, bytes].concat()
}

pub fn decode_msg(data: &[u8]) -> Result<Option<morty_message::Msg>, anyhow::Error> {
    let crc = data[1];
    let msg_data = &data[2..];

    let mut crc8 = Crc8::create_msb(0x07);
    let calc_crc = crc8.calc(msg_data, msg_data.len() as i32, 0);

    if crc != calc_crc {
        error!("Invalid CRC: {} != {}", crc, calc_crc);
        return Err(anyhow!("Invalid CRC: {} != {}", crc, calc_crc));
    }
    let msg = MortyMessage::decode(msg_data)?.msg;

    Ok(msg)
}

pub fn mac_to_string(mac: &[u8]) -> String {
    let mut mac_str = String::new();
    for i in 0..mac.len() {
        mac_str.push_str(&format!("{:02x}", mac[i]));
        if i < mac.len() - 1 {
            mac_str.push(':');
        }
    }
    mac_str
}

pub fn start_wifi(
    modem: esp_idf_hal::modem::Modem,
    sysloop: EspSystemEventLoop,
    ssid: &str,
    password: &str,
) -> Result<Box<EspWifi<'static>>, anyhow::Error> {
    let mut wifi = Box::new(EspWifi::new(modem, sysloop.clone(), None)?);
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.into(),
        password: password.into(),
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

    Ok(wifi)
}
