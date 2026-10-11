//! Reading and writing `.bcm` media fixtures
//!
//! A `.bcm` file is a recording of BcMedia frames as they arrived from a camera (see
//! `examples/bcmedia_dump.rs`) or as produced by `cargo xtask mkfixture`. Tests replay
//! them with their original or accelerated timing.
//!
//! The format is little endian, one record per frame:
//! `u64 host_us | u8 kind | u32 cam_us | u32 cam_time | u32 len | data`
//!
//! - `host_us` is the arrival time in µs since the start of the capture.
//! - `kind` is a [`Kind`]; the high bit is set for H.265 video.
//! - `cam_us` is the camera's µs timestamp (video only) and `cam_time` its wall clock in
//!   unix seconds (I-frames only, 0 when absent).
//! - For Info frames `data` is `u32 width | u32 height | u8 fps`.

use std::convert::TryInto;
use std::io::{self, Read, Write};

/// Size of a record header in bytes
pub const HEADER_LEN: usize = 21;

/// Largest record accepted when reading (a 4K IRAP is under 1 MB)
pub const MAX_RECORD_LEN: usize = 16 * 1024 * 1024;

/// High bit of the kind byte, set for H.265 video
pub const H265_FLAG: u8 = 0x80;

/// What a record holds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// BcMedia InfoV1
    InfoV1,
    /// BcMedia InfoV2
    InfoV2,
    /// Video I-frame (one access unit, Annex-B)
    Iframe,
    /// Video P-frame (one access unit, Annex-B)
    Pframe,
    /// AAC ADTS frame
    Aac,
    /// ADPCM block
    Adpcm,
    /// The source restarted (reconnect); no data
    Restarted,
}

impl Kind {
    fn to_byte(self) -> u8 {
        match self {
            Kind::InfoV1 => 1,
            Kind::InfoV2 => 2,
            Kind::Iframe => 3,
            Kind::Pframe => 4,
            Kind::Aac => 5,
            Kind::Adpcm => 6,
            Kind::Restarted => 0x7f,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Kind::InfoV1,
            2 => Kind::InfoV2,
            3 => Kind::Iframe,
            4 => Kind::Pframe,
            5 => Kind::Aac,
            6 => Kind::Adpcm,
            0x7f => Kind::Restarted,
            _ => return None,
        })
    }

    /// True for I-frames and P-frames
    pub fn is_video(self) -> bool {
        matches!(self, Kind::Iframe | Kind::Pframe)
    }
}

/// One fixture record
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Arrival time, µs since the start of the capture
    pub host_us: u64,
    /// What the record holds
    pub kind: Kind,
    /// Video is H.265 (only meaningful for video records)
    pub h265: bool,
    /// Camera µs timestamp
    pub cam_us: u32,
    /// Camera wall clock, unix seconds, 0 when absent
    pub cam_time: u32,
    /// Payload
    pub data: Vec<u8>,
}

impl Record {
    /// Write the record
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let kind = self.kind.to_byte() | if self.h265 { H265_FLAG } else { 0 };
        w.write_all(&self.host_us.to_le_bytes())?;
        w.write_all(&[kind])?;
        w.write_all(&self.cam_us.to_le_bytes())?;
        w.write_all(&self.cam_time.to_le_bytes())?;
        w.write_all(&(self.data.len() as u32).to_le_bytes())?;
        w.write_all(&self.data)
    }

    /// Read one record, `Ok(None)` at a clean end of file
    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        let mut header = [0u8; HEADER_LEN];
        let mut got = 0;
        while got < HEADER_LEN {
            match r.read(&mut header[got..])? {
                0 if got == 0 => return Ok(None),
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => got += n,
            }
        }
        let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
        let host_us = u64::from_le_bytes(header[0..8].try_into().unwrap());
        let kind = Kind::from_byte(header[8] & !H265_FLAG).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown record kind {:#x}", header[8]),
            )
        })?;
        let len = u32_at(17) as usize;
        if len > MAX_RECORD_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("record of {} bytes is larger than {}", len, MAX_RECORD_LEN),
            ));
        }
        let mut data = vec![0u8; len];
        r.read_exact(&mut data)?;
        Ok(Some(Record {
            host_us,
            kind,
            h265: header[8] & H265_FLAG != 0,
            cam_us: u32_at(9),
            cam_time: u32_at(13),
            data,
        }))
    }
}

/// Read every record of a fixture
pub fn read_all<R: Read>(mut r: R) -> io::Result<Vec<Record>> {
    let mut out = vec![];
    while let Some(rec) = Record::read_from(&mut r)? {
        out.push(rec);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let recs = vec![
            Record {
                host_us: 1,
                kind: Kind::Iframe,
                h265: true,
                cam_us: u32::MAX,
                cam_time: 1_700_000_000,
                data: vec![0, 0, 1, 0x40, 1],
            },
            Record {
                host_us: 2,
                kind: Kind::Restarted,
                h265: false,
                cam_us: 0,
                cam_time: 0,
                data: vec![],
            },
            Record {
                host_us: 3,
                kind: Kind::Aac,
                h265: false,
                cam_us: 0,
                cam_time: 0,
                data: vec![0xff, 0xf1],
            },
        ];
        let mut buf = vec![];
        for r in &recs {
            r.write_to(&mut buf).unwrap();
        }
        assert_eq!(buf[8], 3 | H265_FLAG);
        assert_eq!(read_all(buf.as_slice()).unwrap(), recs);
    }

    #[test]
    fn test_truncated() {
        let mut buf = vec![];
        Record {
            host_us: 0,
            kind: Kind::Pframe,
            h265: false,
            cam_us: 0,
            cam_time: 0,
            data: vec![1, 2, 3],
        }
        .write_to(&mut buf)
        .unwrap();
        buf.pop();
        assert!(read_all(buf.as_slice()).is_err());
        assert!(read_all(&buf[..10]).is_err());
    }
}
