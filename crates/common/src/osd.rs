use std::time::Duration;

use crate::theme::pack::ScreenId;

pub const OSD_DURATION: Duration = Duration::from_millis(1000);
pub const OSD_EVENT_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdKind {
    Volume,
    Brightness,
}

impl OsdKind {
    pub fn screen(self) -> ScreenId {
        match self {
            Self::Volume => ScreenId::OsdVolume,
            Self::Brightness => ScreenId::OsdBrightness,
        }
    }

    pub fn title_key(self, direction: i8) -> &'static str {
        match (self, direction.is_negative()) {
            (Self::Volume, false) => "hotkeys-volume-up",
            (Self::Volume, true) => "hotkeys-volume-down",
            (Self::Brightness, false) => "hotkeys-brightness-up",
            (Self::Brightness, true) => "hotkeys-brightness-down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsdEvent {
    pub kind: OsdKind,
    pub percent: u8,
    pub direction: i8,
    pub seq: u64,
}

impl OsdEvent {
    pub fn new(kind: OsdKind, fraction: f32, direction: i32, seq: u64) -> Self {
        Self {
            kind,
            percent: (fraction.clamp(0.0, 1.0) * 100.0).round() as u8,
            direction: direction.signum() as i8,
            seq,
        }
    }

    pub fn encode(self) -> [u8; OSD_EVENT_BYTES] {
        let mut bytes = [0; OSD_EVENT_BYTES];
        bytes[..4].copy_from_slice(b"AOSD");
        bytes[4] = 1;
        bytes[5] = match self.kind {
            OsdKind::Volume => 0,
            OsdKind::Brightness => 1,
        };
        bytes[6] = self.percent;
        bytes[7] = self.direction as u8;
        bytes[8..].copy_from_slice(&self.seq.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != OSD_EVENT_BYTES
            || &bytes[..4] != b"AOSD"
            || bytes[4] != 1
            || bytes[6] > 100
            || !(-1..=1).contains(&(bytes[7] as i8))
        {
            return None;
        }
        Some(Self {
            kind: match bytes[5] {
                0 => OsdKind::Volume,
                1 => OsdKind::Brightness,
                _ => return None,
            },
            percent: bytes[6],
            direction: bytes[7] as i8,
            seq: u64::from_le_bytes(bytes[8..].try_into().ok()?),
        })
    }

    #[cfg(unix)]
    pub fn send(self) -> std::io::Result<()> {
        self.send_to(crate::constants::LAUNCHER_OSD_SOCKET)
    }

    #[cfg(unix)]
    pub fn send_to(self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let socket = std::os::unix::net::UnixDatagram::unbound()?;
        socket.set_nonblocking(true)?;
        socket.send_to(&self.encode(), path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip_and_level_clamping() {
        for kind in [OsdKind::Volume, OsdKind::Brightness] {
            for (fraction, percent) in [(-0.1, 0), (0.575, 58), (1.1, 100)] {
                for direction in [-50, 0, 50] {
                    let event = OsdEvent::new(kind, fraction, direction, u64::MAX);
                    assert_eq!(event.percent, percent);
                    assert_eq!(event.direction, direction.signum() as i8);
                    assert_eq!(OsdEvent::decode(&event.encode()), Some(event));
                }
            }
        }
    }

    #[test]
    fn malformed_events_are_rejected() {
        let bytes = OsdEvent::new(OsdKind::Volume, 0.4, -1, 1).encode();
        for length in 0..OSD_EVENT_BYTES {
            assert!(OsdEvent::decode(&bytes[..length]).is_none());
        }
        assert!(OsdEvent::decode(&[0; OSD_EVENT_BYTES + 1]).is_none());
        for (offset, value) in [(0, 0), (4, 2), (5, 2), (6, 101), (7, 2)] {
            let mut invalid = bytes;
            invalid[offset] = value;
            assert!(OsdEvent::decode(&invalid).is_none());
        }
    }
}
