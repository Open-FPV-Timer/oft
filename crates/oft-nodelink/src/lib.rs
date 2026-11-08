//! OFT Node link (spec §5): the point-to-point link between one Node and its Timer.
//!
//! Wire format of one frame:
//!
//! ```text
//! COBS( payload || crc16_le(payload) ) 0x00
//! payload = version:u8  type:u8  body...
//! ```
//!
//! - All integers are little-endian.
//! - CRC is CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF) over the payload.
//! - `0x00` delimits frames; COBS guarantees it never appears inside one.
//!
//! `no_std`, no allocation. Messages with variable-length data use fixed-capacity arrays.

#![no_std]
#![forbid(unsafe_code)]

pub mod cobs;
pub mod crc;
pub mod frame;
pub mod msg;

pub use frame::{encode_frame, FrameDecoder, FrameError};
pub use msg::{DecodeError, Message};

/// Node link protocol version carried in every payload.
pub const LINK_VERSION: u8 = 1;

/// Largest payload (version + type + body), before CRC and COBS.
pub const MAX_PAYLOAD: usize = 600;

/// Largest encoded frame including COBS overhead and the trailing delimiter.
pub const MAX_FRAME: usize = cobs::max_encoded_len(MAX_PAYLOAD + 2) + 1;
