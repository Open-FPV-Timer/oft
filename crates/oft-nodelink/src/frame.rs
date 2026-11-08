//! Frame layer: payload + CRC, COBS-encoded, `0x00`-delimited.

use crate::cobs::{self, CobsError};
use crate::crc::crc16;
use crate::msg::{DecodeError, Message};
use crate::{MAX_FRAME, MAX_PAYLOAD};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Frame longer than `MAX_FRAME`; bytes discarded until the next delimiter.
    TooLong,
    /// COBS decoding failed.
    Cobs(CobsError),
    /// Frame shorter than version + type + CRC.
    TooShort,
    /// CRC mismatch.
    Crc,
    /// CRC was fine but the payload did not parse.
    Decode(DecodeError),
    /// Output buffer too small when encoding.
    Overflow,
}

/// Encode `msg` as one complete frame (including trailing `0x00`) into `out`.
pub fn encode_frame(msg: &Message, out: &mut [u8]) -> Result<usize, FrameError> {
    let mut payload = [0u8; MAX_PAYLOAD + 2];
    let n = msg.encode(&mut payload[..MAX_PAYLOAD]).map_err(|_| FrameError::Overflow)?;
    let crc = crc16(&payload[..n]);
    payload[n..n + 2].copy_from_slice(&crc.to_le_bytes());
    let len = cobs::encode(&payload[..n + 2], out).map_err(|_| FrameError::Overflow)?;
    if len >= out.len() {
        return Err(FrameError::Overflow);
    }
    out[len] = 0;
    Ok(len + 1)
}

/// Streaming decoder. Feed bytes as they arrive from the UART or USB.
pub struct FrameDecoder {
    buf: [u8; MAX_FRAME],
    len: usize,
    overflowed: bool,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub const fn new() -> Self {
        Self { buf: [0; MAX_FRAME], len: 0, overflowed: false }
    }

    /// Push one byte. Returns `Some` when a delimiter completes a frame.
    /// Empty frames (back-to-back delimiters) are ignored.
    pub fn push(&mut self, byte: u8) -> Option<Result<Message, FrameError>> {
        if byte != 0 {
            if self.len < self.buf.len() {
                self.buf[self.len] = byte;
                self.len += 1;
            } else {
                self.overflowed = true;
            }
            return None;
        }
        let len = core::mem::replace(&mut self.len, 0);
        if core::mem::replace(&mut self.overflowed, false) {
            return Some(Err(FrameError::TooLong));
        }
        if len == 0 {
            return None;
        }
        Some(Self::finish(&self.buf[..len]))
    }

    fn finish(encoded: &[u8]) -> Result<Message, FrameError> {
        let mut raw = [0u8; MAX_FRAME];
        let n = cobs::decode(encoded, &mut raw).map_err(FrameError::Cobs)?;
        if n < 4 {
            return Err(FrameError::TooShort);
        }
        let (payload, crc_bytes) = raw[..n].split_at(n - 2);
        let crc = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        if crc16(payload) != crc {
            return Err(FrameError::Crc);
        }
        Message::decode(payload).map_err(FrameError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::*;

    fn all_messages() -> [Message; 10] {
        let mut samples = Samples::new(3, 1_234_567_890, 1000, -920, SampleFlags::POST_HOP);
        for v in [-912, -910, -640, -235, 0, 1] {
            samples.push(v).unwrap();
        }
        let mut plan = Plan::with(0, 8000, GAIN_AUTO);
        plan.push(0, 5800).unwrap();
        plan.push(1, 5740).unwrap();
        [
            Message::Hello(Hello {
                chip_id: 0x0011_2233_4455,
                fw: [0, 1, 2],
                link_version: crate::LINK_VERSION,
                freq_min_mhz: 5645,
                freq_max_mhz: 5945,
                max_seats: 8,
                features: Features::SYNC,
            }),
            Message::Plan(plan),
            Message::Samples(samples),
            Message::SyncEdge(SyncEdge { edge_seq: 7, t_node: u64::MAX - 1 }),
            Message::Ping(Ping { id: 9, t_timer: 42 }),
            Message::Pong(Pong { id: 9, t_timer: 42, t_node: 4242 }),
            Message::Status(Status { temp_dc: 413, hop_errors: 1, overruns: 2, gain_index: 60 }),
            Message::Reset,
            Message::Identify { duration_ms: 3000 },
            Message::Dfu,
        ]
    }

    #[test]
    fn roundtrip_every_message_through_a_byte_stream() {
        let mut stream = [0u8; 8192];
        let mut n = 0;
        // Leading delimiter, as a receiver joining mid-stream would see.
        stream[n] = 0;
        n += 1;
        for m in all_messages().iter() {
            n += encode_frame(m, &mut stream[n..]).unwrap();
        }
        let mut dec = FrameDecoder::new();
        let mut got = 0;
        let expected = all_messages();
        for &b in &stream[..n] {
            if let Some(r) = dec.push(b) {
                assert_eq!(r.unwrap(), expected[got]);
                got += 1;
            }
        }
        assert_eq!(got, expected.len());
    }

    #[test]
    fn corrupted_byte_fails_crc_then_recovers() {
        let mut buf = [0u8; 256];
        let msgs = all_messages();
        let n1 = encode_frame(&msgs[3], &mut buf).unwrap();
        let n2 = encode_frame(&msgs[4], &mut buf[n1..]).unwrap();
        buf[2] ^= 0x40; // flip a bit inside the first frame (never creates a zero here)
        let mut dec = FrameDecoder::new();
        let results: [Option<Result<Message, FrameError>>; 2] = {
            let mut out = [None, None];
            let mut k = 0;
            for &b in &buf[..n1 + n2] {
                if let Some(r) = dec.push(b) {
                    out[k] = Some(r);
                    k += 1;
                }
            }
            out
        };
        assert!(matches!(results[0], Some(Err(FrameError::Crc)) | Some(Err(FrameError::Cobs(_)))));
        assert_eq!(results[1], Some(Ok(msgs[4].clone())));
    }

    #[test]
    fn overlong_frame_is_dropped() {
        let mut dec = FrameDecoder::new();
        for _ in 0..MAX_FRAME + 10 {
            assert!(dec.push(0x55).is_none());
        }
        assert_eq!(dec.push(0), Some(Err(FrameError::TooLong)));
        // Decoder is usable again.
        let mut buf = [0u8; 64];
        let n = encode_frame(&Message::Reset, &mut buf).unwrap();
        let mut last = None;
        for &b in &buf[..n] {
            last = dec.push(b).or(last);
        }
        assert_eq!(last, Some(Ok(Message::Reset)));
    }

    #[test]
    fn largest_samples_frame_fits() {
        let mut s = Samples::new(0, 0, 1000, 0, SampleFlags::empty());
        while s.push(0).is_ok() {}
        assert_eq!(s.len(), MAX_SAMPLES);
        let mut buf = [0u8; MAX_FRAME];
        let n = encode_frame(&Message::Samples(s), &mut buf).unwrap();
        assert!(n <= MAX_FRAME);
    }
}
