//! Node link messages (spec §5). One `Message` per frame.
//!
//! Body layouts are documented on each struct. All integers little-endian.
//! Times are µs on the sender's own clock (`t_node` = Node clock, `t_timer` = Timer clock).

use crate::LINK_VERSION;

/// Message type codes. Odd = Node → Timer, even = Timer → Node (with `Ping`/`Pong` paired).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hello = 0x01,
    Plan = 0x02,
    Samples = 0x03,
    SyncEdge = 0x05,
    Ping = 0x06,
    Pong = 0x07,
    Status = 0x09,
    Reset = 0x10,
    Identify = 0x12,
    Dfu = 0x14,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Payload too short for version + type, or for the body of that type.
    Short,
    UnknownType(u8),
    /// Link version not understood.
    Version(u8),
    /// A count field exceeded its fixed capacity.
    Capacity,
    /// Output buffer too small (encode side).
    Overflow,
}

// ---------------------------------------------------------------- Hello

/// Feature bits a Node advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features(pub u16);

impl Features {
    /// Node has a SYNC input and emits `SyncEdge`.
    pub const SYNC: Features = Features(1 << 0);
    /// Node can dump raw I/Q (debug).
    pub const IQ_DUMP: Features = Features(1 << 1);
    /// Node can reboot into its ROM bootloader on `Dfu`.
    pub const DFU: Features = Features(1 << 2);

    pub const fn empty() -> Self {
        Features(0)
    }
    pub const fn contains(self, other: Features) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for Features {
    type Output = Features;
    fn bitor(self, rhs: Self) -> Self {
        Features(self.0 | rhs.0)
    }
}

/// Node → Timer on boot and on request.
///
/// Body: `chip_id:u64 fw:[u8;3] link_version:u8 freq_min:u16 freq_max:u16 max_seats:u8 features:u16`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub chip_id: u64,
    /// Firmware version major, minor, patch.
    pub fw: [u8; 3],
    pub link_version: u8,
    pub freq_min_mhz: u16,
    pub freq_max_mhz: u16,
    pub max_seats: u8,
    pub features: Features,
}

// ---------------------------------------------------------------- Plan

pub const MAX_PLAN_SEATS: usize = 8;
/// `gain_index` value meaning "let the Node pick".
pub const GAIN_AUTO: u8 = 0xFF;

/// Timer → Node: which frequencies to watch.
///
/// Body: `dwell_us:u32 filter_bw_khz:u16 gain_index:u8 n:u8 (seat:u8 freq_mhz:u16)*n`
///
/// With one seat the Node dwells continuously. With more it hops in list order, staying
/// `dwell_us` on each (0 = Node default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub dwell_us: u32,
    pub filter_bw_khz: u16,
    pub gain_index: u8,
    n: u8,
    seats: [(u8, u16); MAX_PLAN_SEATS],
}

impl Default for Plan {
    fn default() -> Self {
        Self { dwell_us: 0, filter_bw_khz: 0, gain_index: GAIN_AUTO, n: 0, seats: [(0, 0); MAX_PLAN_SEATS] }
    }
}

impl Plan {
    pub fn with(dwell_us: u32, filter_bw_khz: u16, gain_index: u8) -> Self {
        Self { dwell_us, filter_bw_khz, gain_index, ..Self::default() }
    }
    pub fn push(&mut self, seat: u8, freq_mhz: u16) -> Result<(), DecodeError> {
        if self.n as usize >= MAX_PLAN_SEATS {
            return Err(DecodeError::Capacity);
        }
        self.seats[self.n as usize] = (seat, freq_mhz);
        self.n += 1;
        Ok(())
    }
    pub fn seats(&self) -> &[(u8, u16)] {
        &self.seats[..self.n as usize]
    }
}

// ---------------------------------------------------------------- Samples

pub const MAX_SAMPLES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleFlags(pub u8);

impl SampleFlags {
    /// ADC or gain stage saturated during this batch.
    pub const CLIPPED: SampleFlags = SampleFlags(1 << 0);
    /// First samples after a frequency hop; may be unsettled.
    pub const POST_HOP: SampleFlags = SampleFlags(1 << 1);

    pub const fn empty() -> Self {
        SampleFlags(0)
    }
    pub const fn contains(self, o: SampleFlags) -> bool {
        self.0 & o.0 == o.0
    }
}

/// Node → Timer: a batch of band-power samples for one seat.
///
/// Body: `seat:u8 t0:u64 dt_us:u16 noise_floor:i16 flags:u8 n:u16 (power:i16)*n`
///
/// Power and noise floor are in **0.1 dB**, relative unless the Node is calibrated.
/// Sample `k` was taken at `t0 + k*dt_us` on the Node clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Samples {
    pub seat: u8,
    pub t0: u64,
    pub dt_us: u16,
    pub noise_floor: i16,
    pub flags: SampleFlags,
    n: u16,
    power: [i16; MAX_SAMPLES],
}

impl Samples {
    pub fn new(seat: u8, t0: u64, dt_us: u16, noise_floor: i16, flags: SampleFlags) -> Self {
        Self { seat, t0, dt_us, noise_floor, flags, n: 0, power: [0; MAX_SAMPLES] }
    }
    pub fn push(&mut self, p: i16) -> Result<(), DecodeError> {
        if self.n as usize >= MAX_SAMPLES {
            return Err(DecodeError::Capacity);
        }
        self.power[self.n as usize] = p;
        self.n += 1;
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.n as usize
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn power(&self) -> &[i16] {
        &self.power[..self.n as usize]
    }
    /// Node-clock time of sample `k`.
    pub fn time_of(&self, k: usize) -> u64 {
        self.t0 + (k as u64) * (self.dt_us as u64)
    }
}

// ---------------------------------------------------------------- small messages

/// Node → Timer: the Node clock when a SYNC edge arrived. Body: `edge_seq:u32 t_node:u64`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncEdge {
    pub edge_seq: u32,
    pub t_node: u64,
}

/// Timer → Node. Body: `id:u32 t_timer:u64`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub id: u32,
    pub t_timer: u64,
}

/// Node → Timer, answering `Ping` as fast as possible. Body: `id:u32 t_timer:u64 t_node:u64`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pong {
    pub id: u32,
    pub t_timer: u64,
    pub t_node: u64,
}

/// Node → Timer, periodic. Body: `temp_dc:i16 hop_errors:u16 overruns:u16 gain_index:u8`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Temperature in 0.1 °C. `i16::MIN` = no reading.
    pub temp_dc: i16,
    pub hop_errors: u16,
    pub overruns: u16,
    pub gain_index: u8,
}

// ---------------------------------------------------------------- Message

/// `Samples` is ~530 bytes and the rest are tiny; that is deliberate (no allocation).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Plan(Plan),
    Samples(Samples),
    SyncEdge(SyncEdge),
    Ping(Ping),
    Pong(Pong),
    Status(Status),
    /// Timer → Node: reboot. No body.
    Reset,
    /// Timer → Node: blink the LED. Body: `duration_ms:u16`
    Identify { duration_ms: u16 },
    /// Timer → Node: reboot into the ROM bootloader. No body.
    Dfu,
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn put(&mut self, b: &[u8]) -> Result<(), DecodeError> {
        let end = self.pos + b.len();
        if end > self.buf.len() {
            return Err(DecodeError::Overflow);
        }
        self.buf[self.pos..end].copy_from_slice(b);
        self.pos = end;
        Ok(())
    }
    fn u8(&mut self, v: u8) -> Result<(), DecodeError> {
        self.put(&[v])
    }
    fn u16(&mut self, v: u16) -> Result<(), DecodeError> {
        self.put(&v.to_le_bytes())
    }
    fn i16(&mut self, v: i16) -> Result<(), DecodeError> {
        self.put(&v.to_le_bytes())
    }
    fn u32(&mut self, v: u32) -> Result<(), DecodeError> {
        self.put(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) -> Result<(), DecodeError> {
        self.put(&v.to_le_bytes())
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self.pos + N;
        if end > self.buf.len() {
            return Err(DecodeError::Short);
        }
        let mut a = [0u8; N];
        a.copy_from_slice(&self.buf[self.pos..end]);
        self.pos = end;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take()?))
    }
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::Hello(_) => Kind::Hello,
            Message::Plan(_) => Kind::Plan,
            Message::Samples(_) => Kind::Samples,
            Message::SyncEdge(_) => Kind::SyncEdge,
            Message::Ping(_) => Kind::Ping,
            Message::Pong(_) => Kind::Pong,
            Message::Status(_) => Kind::Status,
            Message::Reset => Kind::Reset,
            Message::Identify { .. } => Kind::Identify,
            Message::Dfu => Kind::Dfu,
        }
    }

    /// Encode payload (version, type, body) into `out`. Returns the length.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, DecodeError> {
        let mut w = Writer { buf: out, pos: 0 };
        w.u8(LINK_VERSION)?;
        w.u8(self.kind() as u8)?;
        match self {
            Message::Hello(h) => {
                w.u64(h.chip_id)?;
                w.put(&h.fw)?;
                w.u8(h.link_version)?;
                w.u16(h.freq_min_mhz)?;
                w.u16(h.freq_max_mhz)?;
                w.u8(h.max_seats)?;
                w.u16(h.features.0)?;
            }
            Message::Plan(p) => {
                w.u32(p.dwell_us)?;
                w.u16(p.filter_bw_khz)?;
                w.u8(p.gain_index)?;
                w.u8(p.n)?;
                for &(seat, f) in p.seats() {
                    w.u8(seat)?;
                    w.u16(f)?;
                }
            }
            Message::Samples(s) => {
                w.u8(s.seat)?;
                w.u64(s.t0)?;
                w.u16(s.dt_us)?;
                w.i16(s.noise_floor)?;
                w.u8(s.flags.0)?;
                w.u16(s.n)?;
                for &p in s.power() {
                    w.i16(p)?;
                }
            }
            Message::SyncEdge(e) => {
                w.u32(e.edge_seq)?;
                w.u64(e.t_node)?;
            }
            Message::Ping(p) => {
                w.u32(p.id)?;
                w.u64(p.t_timer)?;
            }
            Message::Pong(p) => {
                w.u32(p.id)?;
                w.u64(p.t_timer)?;
                w.u64(p.t_node)?;
            }
            Message::Status(s) => {
                w.i16(s.temp_dc)?;
                w.u16(s.hop_errors)?;
                w.u16(s.overruns)?;
                w.u8(s.gain_index)?;
            }
            Message::Reset | Message::Dfu => {}
            Message::Identify { duration_ms } => w.u16(*duration_ms)?,
        }
        Ok(w.pos)
    }

    /// Decode a payload (version, type, body).
    pub fn decode(payload: &[u8]) -> Result<Message, DecodeError> {
        let mut r = Reader { buf: payload, pos: 0 };
        let version = r.u8()?;
        if version != LINK_VERSION {
            return Err(DecodeError::Version(version));
        }
        let kind = r.u8()?;
        let msg = match kind {
            x if x == Kind::Hello as u8 => Message::Hello(Hello {
                chip_id: r.u64()?,
                fw: r.take()?,
                link_version: r.u8()?,
                freq_min_mhz: r.u16()?,
                freq_max_mhz: r.u16()?,
                max_seats: r.u8()?,
                features: Features(r.u16()?),
            }),
            x if x == Kind::Plan as u8 => {
                let mut p = Plan { dwell_us: r.u32()?, filter_bw_khz: r.u16()?, gain_index: r.u8()?, ..Plan::default() };
                let n = r.u8()?;
                for _ in 0..n {
                    let seat = r.u8()?;
                    let f = r.u16()?;
                    p.push(seat, f)?;
                }
                Message::Plan(p)
            }
            x if x == Kind::Samples as u8 => {
                let seat = r.u8()?;
                let t0 = r.u64()?;
                let dt_us = r.u16()?;
                let nf = r.i16()?;
                let flags = SampleFlags(r.u8()?);
                let n = r.u16()?;
                let mut s = Samples::new(seat, t0, dt_us, nf, flags);
                for _ in 0..n {
                    let p = r.i16()?;
                    s.push(p)?;
                }
                Message::Samples(s)
            }
            x if x == Kind::SyncEdge as u8 => Message::SyncEdge(SyncEdge { edge_seq: r.u32()?, t_node: r.u64()? }),
            x if x == Kind::Ping as u8 => Message::Ping(Ping { id: r.u32()?, t_timer: r.u64()? }),
            x if x == Kind::Pong as u8 => Message::Pong(Pong { id: r.u32()?, t_timer: r.u64()?, t_node: r.u64()? }),
            x if x == Kind::Status as u8 => Message::Status(Status {
                temp_dc: r.i16()?,
                hop_errors: r.u16()?,
                overruns: r.u16()?,
                gain_index: r.u8()?,
            }),
            x if x == Kind::Reset as u8 => Message::Reset,
            x if x == Kind::Identify as u8 => Message::Identify { duration_ms: r.u16()? },
            x if x == Kind::Dfu as u8 => Message::Dfu,
            other => return Err(DecodeError::UnknownType(other)),
        };
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_other_versions_and_unknown_types() {
        assert_eq!(Message::decode(&[LINK_VERSION + 1, 0x10]), Err(DecodeError::Version(LINK_VERSION + 1)));
        assert_eq!(Message::decode(&[LINK_VERSION, 0x7F]), Err(DecodeError::UnknownType(0x7F)));
        assert_eq!(Message::decode(&[LINK_VERSION]), Err(DecodeError::Short));
    }

    #[test]
    fn truncated_body_is_short() {
        let mut buf = [0u8; 64];
        let n = Message::Ping(Ping { id: 1, t_timer: 2 }).encode(&mut buf).unwrap();
        assert_eq!(Message::decode(&buf[..n - 1]), Err(DecodeError::Short));
    }

    #[test]
    fn samples_count_above_capacity_is_rejected() {
        // seat, t0, dt, nf, flags, n = 300
        let mut buf = [0u8; 2048];
        let mut w = Writer { buf: &mut buf, pos: 0 };
        w.u8(LINK_VERSION).unwrap();
        w.u8(Kind::Samples as u8).unwrap();
        w.u8(0).unwrap();
        w.u64(0).unwrap();
        w.u16(1000).unwrap();
        w.i16(0).unwrap();
        w.u8(0).unwrap();
        w.u16(300).unwrap();
        for _ in 0..300 {
            w.i16(0).unwrap();
        }
        let n = w.pos;
        assert_eq!(Message::decode(&buf[..n]), Err(DecodeError::Capacity));
    }

    #[test]
    fn sample_times() {
        let s = Samples::new(0, 1_000_000, 1000, 0, SampleFlags::empty());
        assert_eq!(s.time_of(0), 1_000_000);
        assert_eq!(s.time_of(5), 1_005_000);
    }
}
