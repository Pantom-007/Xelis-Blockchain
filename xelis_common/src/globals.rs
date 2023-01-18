use crate::serializer::{Reader, ReaderError};
use crate::config::{COIN_VALUE, FEE_PER_KB};
use std::time::{SystemTime, UNIX_EPOCH};
use std::net::{SocketAddr, IpAddr, Ipv4Addr, Ipv6Addr};

// return timestamp in seconds
pub fn get_current_time() -> u64 {
    let start = SystemTime::now();
    let time = start.duration_since(UNIX_EPOCH).expect("Incorrect time returned from get_current_time");
    time.as_secs()
}

// return timestamp in milliseconds
pub fn get_current_timestamp() -> u128 {
    let start = SystemTime::now();
    let time = start.duration_since(UNIX_EPOCH).expect("Incorrect time returned from get_current_timestamp");
    time.as_millis()
}

pub fn format_coin(value: u64) -> String {
    format!("{}", value as f64 / COIN_VALUE as f64)
}

// format a IP:port to byte format
pub fn ip_to_bytes(ip: &SocketAddr) -> Vec<u8> {
    let mut bytes = Vec::new();
    match ip.ip() {
        IpAddr::V4(addr) => {
            bytes.push(0);
            bytes.extend(addr.octets());
        },
        IpAddr::V6(addr) => {
            bytes.push(1);
            bytes.extend(addr.octets());
        }
    };
    bytes.extend(ip.port().to_be_bytes());
    bytes
}

// bytes to ip
pub fn ip_from_bytes(reader: &mut Reader) -> Result<SocketAddr, ReaderError> {
    let is_v6 = reader.read_bool()?;
    let ip: IpAddr = if !is_v6 {
        let a = reader.read_u8()?;
        let b = reader.read_u8()?;
        let c = reader.read_u8()?;
        let d = reader.read_u8()?;
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    } else {
        let a = reader.read_u16()?;
        let b = reader.read_u16()?;
        let c = reader.read_u16()?;
        let d = reader.read_u16()?;
        let e = reader.read_u16()?;
        let f = reader.read_u16()?;
        let g = reader.read_u16()?;
        let h = reader.read_u16()?;
        IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h))
    };
    let port = reader.read_u16()?;
    Ok(SocketAddr::new(ip, port))
}

pub fn calculate_tx_fee(tx_size: usize) -> u64 {
    let mut size_in_kb = tx_size as u64 / 1024;

    if tx_size % 1024 != 0 { // we consume a full kb for fee
        size_in_kb += 1;
    }
    
    size_in_kb * FEE_PER_KB
}

const HASHRATE_FORMATS: [&str; 5] = ["H/s", "KH/s", "MH/s", "GH/s", "TH/s"];

pub fn format_hashrate(mut hashrate: f64) -> String {
    let max = HASHRATE_FORMATS.len();
    let mut count = 0;
    while hashrate > 1000f64 && count < max {
        count += 1;
        hashrate = hashrate / 1000f64;
    }

    return format!("{} {}", hashrate, HASHRATE_FORMATS[count]);
}