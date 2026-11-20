//! USB driver for the Thermalright Trofeo Vision 9.16" LCD (LY bulk protocol).
//!
//! Wire format (little endian):
//! * Handshake: 2048-byte request, 512-byte reply. The reply carries the panel
//!   mode (PM) and sub-type; PM maps to a framebuffer layout (FBL) and resolution.
//! * Frame: a JPEG split into 512-byte packets (16-byte header + up to 496 bytes
//!   of payload), the packet count padded to a multiple of 4, sent over bulk OUT
//!   in transfers of up to 4096 bytes, followed by a 512-byte ACK on bulk IN.

use std::fmt;
use std::thread::sleep;
use std::time::Duration;

use rusb::{DeviceHandle, GlobalContext};

pub const VID: u16 = 0x0416;
pub const PID: u16 = 0x5408;
const EP_OUT: u8 = 0x09;
const EP_IN: u8 = 0x81;
const PACKET: usize = 512;
const PACKET_HEADER: usize = 16;
const PACKET_PAYLOAD: usize = PACKET - PACKET_HEADER; // 496
const MAX_TRANSFER: usize = 4096;
pub const MAX_JPEG: usize = 650 * 1024;

#[derive(Debug)]
pub enum LcdError {
    NotFound,
    Usb(rusb::Error),
    BadReply(Vec<u8>),
    Stalled,
}

impl fmt::Display for LcdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LcdError::NotFound => write!(f, "device {:04x}:{:04x} not found", VID, PID),
            LcdError::Usb(e) => write!(f, "usb: {}", e),
            LcdError::BadReply(r) => {
                let hex: String = r.iter().take(40).map(|b| format!("{:02x}", b)).collect();
                write!(f, "unexpected handshake reply: {}", hex)
            }
            LcdError::Stalled => write!(f, "bulk write made no progress"),
        }
    }
}

impl From<rusb::Error> for LcdError {
    fn from(e: rusb::Error) -> Self {
        LcdError::Usb(e)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PanelInfo {
    pub pm: u8,
    pub sub: u8,
    pub fbl: u8,
    pub width: u32,
    pub height: u32,
}

pub struct Lcd {
    handle: DeviceHandle<GlobalContext>,
}

impl Lcd {
    pub fn open() -> Result<Lcd, LcdError> {
        let handle = rusb::open_device_with_vid_pid(VID, PID).ok_or(LcdError::NotFound)?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        match handle.set_active_configuration(1) {
            Ok(()) | Err(rusb::Error::Busy) => {}
            Err(e) => return Err(e.into()),
        }
        handle.claim_interface(0)?;
        Ok(Lcd { handle })
    }

    /// Query the panel. A freshly powered panel may answer "not ready"
    /// (byte 8 == 0x02) once, so the query is repeated a few times.
    pub fn handshake(&self) -> Result<PanelInfo, LcdError> {
        let mut request = [0u8; 2048];
        request[0] = 0x02;
        request[1] = 0xFF;
        request[8] = 0x01;
        let mut reply = [0u8; PACKET];
        let mut last = Vec::new();
        for _ in 0..5 {
            self.handle.write_bulk(EP_OUT, &request, Duration::from_millis(1000))?;
            let n = self.handle.read_bulk(EP_IN, &mut reply, Duration::from_millis(1000))?;
            let r = &reply[..n];
            if n >= 37 && r[0] == 0x03 && r[1] == 0xFF && r[8] == 0x01 {
                return Ok(decode_panel(r));
            }
            last = r.to_vec();
            sleep(Duration::from_millis(200));
        }
        Err(LcdError::BadReply(last))
    }

    pub fn send_jpeg(&self, jpeg: &[u8]) -> Result<(), LcdError> {
        let stream = packetize(jpeg);
        let mut sent = 0;
        while sent < stream.len() {
            let end = (sent + MAX_TRANSFER).min(stream.len());
            let n = self.handle.write_bulk(EP_OUT, &stream[sent..end], Duration::from_millis(5000))?;
            if n == 0 {
                return Err(LcdError::Stalled);
            }
            sent += n;
        }
        let mut ack = [0u8; PACKET];
        self.handle.read_bulk(EP_IN, &mut ack, Duration::from_millis(1000))?;
        Ok(())
    }
}

impl Drop for Lcd {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(0);
    }
}

fn decode_panel(r: &[u8]) -> PanelInfo {
    let mode = if r[20] <= 3 { 1 } else { r[20] };
    let pm = 64u8.wrapping_add(mode);
    let sub = r[22].wrapping_add(1);
    let fbl = match pm {
        63 | 64 => 114,
        65 | 66 | 68 | 69 => 192,
        other => other,
    };
    let (width, height) = match (fbl, pm) {
        (192, 68) => (1280, 480),
        (192, 69) => (1920, 440),
        (192, _) => (1920, 480),
        (114, _) => (1600, 720),
        (128, _) => (1280, 480),
        _ => (1920, 462),
    };
    PanelInfo { pm, sub, fbl, width, height }
}

/// Build the padded packet stream for one JPEG frame.
fn packetize(jpeg: &[u8]) -> Vec<u8> {
    let total = jpeg.len();
    let count = total.div_ceil(PACKET_PAYLOAD).max(1);
    let padded = count.div_ceil(4) * 4;
    let mut out = vec![0u8; padded * PACKET];
    for (idx, piece) in jpeg.chunks(PACKET_PAYLOAD).enumerate() {
        let pkt = &mut out[idx * PACKET..(idx + 1) * PACKET];
        pkt[0] = 0x01;
        pkt[1] = 0xFF;
        pkt[2..6].copy_from_slice(&(total as u32).to_le_bytes());
        pkt[6..8].copy_from_slice(&(piece.len() as u16).to_le_bytes());
        pkt[8] = 0x01;
        pkt[9..11].copy_from_slice(&(count as u16).to_le_bytes());
        pkt[11..13].copy_from_slice(&(idx as u16).to_le_bytes());
        pkt[PACKET_HEADER..PACKET_HEADER + piece.len()].copy_from_slice(piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_layout() {
        let jpeg = vec![0xAB; 496 * 2];
        let s = packetize(&jpeg);
        assert_eq!(s.len(), 4 * PACKET); // 2 packets padded to 4
        assert_eq!(&s[0..2], &[0x01, 0xFF]);
        assert_eq!(u32::from_le_bytes(s[2..6].try_into().unwrap()), 992);
        assert_eq!(u16::from_le_bytes(s[9..11].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(s[PACKET + 11..PACKET + 13].try_into().unwrap()), 1);
        assert!(s[2 * PACKET..].iter().all(|&b| b == 0));
    }

    #[test]
    fn panel_decode() {
        let mut r = vec![0u8; 512];
        r[20] = 1;
        r[22] = 2;
        let p = decode_panel(&r);
        assert_eq!((p.pm, p.sub, p.fbl, p.width, p.height), (65, 3, 192, 1920, 480));
    }
}
