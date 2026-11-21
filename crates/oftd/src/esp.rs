//! Writing an ESP32-C5 Node's firmware through the chip's ROM bootloader, over the Node
//! link's own wires. The protocol is `oft-esprom`, shared with the RP2350 Timer; the order
//! of steps here is the one that Timer's firmware uses (`oft-timer-rp2350/src/node.rs`).

use crate::stm32::Wire;
use md5::Digest as _;
use oft_esprom as esprom;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The ROM bootloader measures the baud rate from the sync packet; this is its default.
pub const ROM_BAUD: u32 = 115_200;
/// The XIAO ESP32-C5's flash.
const FLASH_BYTES: u32 = 4 * 1024 * 1024;
/// Largest firmware accepted: the application partition of the Node's flash layout.
const MAX_APP: u32 = 0x1F_0000;
/// Where the Node's firmware goes unless the request names another part.
pub const APP_OFFSET: u32 = esprom::C5_APP_OFFSET;

/// Whether `image` is the kind of file that belongs at `offset` on a C5.
pub fn check_image(offset: u32, image: &[u8]) -> Result<(), String> {
    esprom::check_image(offset, image.len() as u32, &image[..image.len().min(16)], MAX_APP).map_err(|e| e.as_str().to_string())
}

struct Rom<'a> {
    wire: &'a mut dyn Wire,
    frame: Box<[u8; esprom::MAX_FRAME]>,
}

impl Rom<'_> {
    /// One request and its reply. Replies to other requests still in flight are skipped.
    fn cmd(&mut self, req: &esprom::Request, timeout: Duration) -> Result<(u32, Vec<u8>), String> {
        self.wire.discard();
        let n = req.frame(&mut self.frame);
        self.wire.write(&self.frame[..n]).map_err(|e| format!("could not write to the serial port: {e}"))?;
        let deadline = Instant::now() + timeout;
        let mut de = esprom::Deframer::<64>::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut b = [0u8];
            if left.is_zero() || self.wire.read(&mut b, left).is_err() {
                return Err("the Node's bootloader did not answer".into());
            }
            if !de.push(b[0]) {
                continue;
            }
            let Some(reply) = esprom::parse_reply(de.frame()) else { continue };
            if reply.op != req.op() {
                continue;
            }
            if reply.status != 0 {
                return Err(esprom::error_text(reply.error).into());
            }
            return Ok((reply.value, reply.data.to_vec()));
        }
    }

    fn sync(&mut self, tries: u32) -> bool {
        (0..tries).any(|_| self.cmd(&esprom::sync(), Duration::from_millis(150)).is_ok())
    }
}

/// Write `image` at `offset` to a chip already restarted into download mode, and check it
/// with the ROM's MD5 of the flash. `progress` gets the step and the bytes written.
pub fn flash(wire: &mut dyn Wire, image: &[u8], offset: u32, progress: &mut dyn FnMut(&'static str, usize)) -> Result<(), String> {
    check_image(offset, image)?;
    let len = image.len() as u32;
    let mut rom = Rom { wire, frame: Box::new([0; esprom::MAX_FRAME]) };

    progress("connecting", 0);
    if !rom.sync(40) {
        return Err("the Node's bootloader did not answer (check the EN and BOOT wires)".into());
    }
    // The ROM answers a sync several times over; let the rest arrive and be discarded.
    sleep(Duration::from_millis(250));

    // Stop the chip's watchdogs from restarting it part way, as esptool does for this chip
    // family. Not fatal if it fails.
    {
        use esprom::c5_wdt as w;
        let t = Duration::from_secs(1);
        for (addr, value) in [(w::RWDT_WPROTECT, w::WKEY), (w::RWDT_CONFIG0, 0), (w::RWDT_WPROTECT, 0), (w::SWD_WPROTECT, w::WKEY)] {
            let _ = rom.cmd(&esprom::write_reg(addr, value), t);
        }
        if let Ok((value, _)) = rom.cmd(&esprom::read_reg(w::SWD_CONFIG), t) {
            let _ = rom.cmd(&esprom::write_reg(w::SWD_CONFIG, value | w::SWD_AUTO_FEED_EN), t);
        }
        let _ = rom.cmd(&esprom::write_reg(w::SWD_WPROTECT, 0), t);
    }

    rom.cmd(&esprom::spi_attach(), Duration::from_secs(3))?;
    rom.cmd(&esprom::spi_set_params(FLASH_BYTES), Duration::from_secs(3))?;

    // Written in sessions. If a block gets no clean answer, the Timer cannot know whether
    // the bootloader wrote it, and the bootloader refuses a block number it has already
    // counted. So after any failure: start a new session at the 4 KiB flash sector the
    // failed block lies in. Starting a session erases from there on, so what is rewritten
    // always lands on erased flash.
    const SECTOR_BLOCKS: usize = 4096 / esprom::BLOCK;
    const MAX_RESTARTS: u32 = 80;
    let blocks: Vec<&[u8]> = image.chunks(esprom::BLOCK).collect();
    let (mut start, mut restarts) = (0usize, 0u32);
    loop {
        let from = start * esprom::BLOCK;
        let left = len - from as u32;
        progress(if start == 0 { "erasing" } else { "writing" }, from);
        let mut ok = rom.cmd(&esprom::flash_begin(left, offset + from as u32), Duration::from_millis(esprom::erase_timeout_ms(left) as u64)).is_ok();
        if ok {
            for (seq, block) in blocks.iter().enumerate().skip(start) {
                if rom.cmd(&esprom::flash_data((seq - start) as u32, block), Duration::from_secs(1)).is_err() {
                    start = seq - seq % SECTOR_BLOCKS;
                    ok = false;
                    break;
                }
                progress("writing", (seq * esprom::BLOCK + block.len()).min(image.len()));
            }
        }
        if ok {
            break;
        }
        restarts += 1;
        if restarts > MAX_RESTARTS {
            return Err("the link to the Node's bootloader is too unreliable to finish the write".into());
        }
        // Usually the bootloader is still listening and only its answer was lost. Call it
        // again (sync) only if two sessions in a row fail to start.
        sleep(Duration::from_millis(50));
        if restarts % 2 == 0 {
            sleep(Duration::from_millis(100));
            if !rom.sync(20) {
                return Err("the Node's bootloader stopped answering part way through".into());
            }
            sleep(Duration::from_millis(250));
        }
    }

    progress("verifying", image.len());
    let (_, got) = rom.cmd(&esprom::spi_flash_md5(offset, len), Duration::from_millis(esprom::md5_timeout_ms(len) as u64))?;
    let want: String = md5::Md5::digest(image).iter().map(|b| format!("{b:02x}")).collect();
    if !got.eq_ignore_ascii_case(want.as_bytes()) {
        return Err("the Node's flash does not match the file after writing".into());
    }
    Ok(())
}

/// A simulated C5 ROM bootloader, for tests.
pub mod sim {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    pub struct SimRom {
        pub flash: Vec<u8>,
        /// Swallow the reply to this many-th `FLASH_DATA` (counted from 1), once.
        pub lose_reply_at: Option<u32>,
        /// Sessions started (`FLASH_BEGIN`s).
        pub sessions: u32,
        data_seen: u32,
        session: Option<(usize, u32)>, // offset, next block
        de: esprom::Deframer<{ esprom::MAX_REQUEST }>,
        tx: VecDeque<u8>,
    }

    impl SimRom {
        pub fn new() -> Self {
            Self { flash: vec![0x5A; FLASH_BYTES as usize], lose_reply_at: None, sessions: 0, data_seen: 0, session: None, de: esprom::Deframer::new(), tx: VecDeque::new() }
        }

        fn reply(&mut self, op: u8, value: u32, data: &[u8], error: u8) {
            let mut raw = vec![0x01, op];
            raw.extend(((data.len() + 4) as u16).to_le_bytes());
            raw.extend(value.to_le_bytes());
            raw.extend(data);
            raw.extend([(error != 0) as u8, error, 0, 0]);
            self.tx.push_back(0xC0);
            for b in raw {
                match b {
                    0xC0 => self.tx.extend([0xDB, 0xDC]),
                    0xDB => self.tx.extend([0xDB, 0xDD]),
                    b => self.tx.push_back(b),
                }
            }
            self.tx.push_back(0xC0);
        }

        fn request(&mut self, r: Vec<u8>) {
            let word = |k: usize| u32::from_le_bytes([r[8 + 4 * k], r[9 + 4 * k], r[10 + 4 * k], r[11 + 4 * k]]);
            let op = r[1];
            match op {
                esprom::OP_SYNC => (0..8).for_each(|_| self.reply(op, 0, &[], 0)),
                esprom::OP_READ_REG => self.reply(op, 0x1234, &[], 0),
                esprom::OP_FLASH_BEGIN => {
                    let (size, offset) = (word(0) as usize, word(3) as usize);
                    let end = (offset + size).div_ceil(4096) * 4096;
                    self.flash[offset..end].fill(0xFF);
                    self.session = Some((offset, 0));
                    self.sessions += 1;
                    self.reply(op, 0, &[], 0);
                }
                esprom::OP_FLASH_DATA => {
                    let (seq, block) = (word(1), &r[24..]);
                    let sum = block.iter().fold(0xEFu8, |a, b| a ^ b) as u32;
                    let Some((offset, next)) = self.session else { return self.reply(op, 0, &[], 0x06) };
                    if sum != u32::from_le_bytes([r[4], r[5], r[6], r[7]]) {
                        return self.reply(op, 0, &[], 0x07);
                    }
                    if seq != next {
                        return self.reply(op, 0, &[], 0x06);
                    }
                    let at = offset + seq as usize * esprom::BLOCK;
                    for (k, b) in block.iter().enumerate() {
                        self.flash[at + k] &= b; // flash only clears bits until erased
                    }
                    self.session = Some((offset, next + 1));
                    self.data_seen += 1;
                    if self.lose_reply_at == Some(self.data_seen) {
                        self.lose_reply_at = None;
                    } else {
                        self.reply(op, 0, &[], 0);
                    }
                }
                esprom::OP_SPI_FLASH_MD5 => {
                    let (offset, size) = (word(0) as usize, word(1) as usize);
                    let hex: String = md5::Md5::digest(&self.flash[offset..offset + size]).iter().map(|b| format!("{b:02x}")).collect();
                    self.reply(op, 0, hex.as_bytes(), 0);
                }
                _ => self.reply(op, 0, &[], 0),
            }
        }
    }

    impl Default for SimRom {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Wire for SimRom {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            for b in data {
                if self.de.push(*b) {
                    let r = self.de.frame().to_vec();
                    self.request(r);
                }
            }
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<()> {
            for b in buf.iter_mut() {
                *b = self.tx.pop_front().ok_or(io::ErrorKind::TimedOut)?;
            }
            Ok(())
        }
        fn discard(&mut self) {
            self.tx.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sim::SimRom;
    use super::*;

    /// Something shaped like a C5 application image, with SLIP's special bytes in it.
    fn firmware(len: usize) -> Vec<u8> {
        let mut f: Vec<u8> = (0..len).map(|k| [0xC0, 0xDB, 0x00, 0x7F][k % 4] ^ (k / 97) as u8).collect();
        f[0] = 0xE9;
        f[12..14].copy_from_slice(&esprom::C5_CHIP_ID.to_le_bytes());
        f
    }

    #[test]
    fn writes_and_verifies() {
        let mut rom = SimRom::new();
        let image = firmware(10_000);
        let mut steps = Vec::new();
        flash(&mut rom, &image, APP_OFFSET, &mut |s, _| {
            if steps.last() != Some(&s) {
                steps.push(s)
            }
        })
        .unwrap();
        let at = APP_OFFSET as usize;
        assert_eq!(rom.flash[at..at + 10_000], image[..]);
        assert_eq!(rom.flash[at - 1], 0x5A, "nothing before the image is touched");
        assert_eq!(rom.sessions, 1);
        assert_eq!(steps, ["connecting", "erasing", "writing", "verifying"]);
    }

    #[test]
    fn a_lost_answer_restarts_at_the_sector_and_still_verifies() {
        let mut rom = SimRom::new();
        rom.lose_reply_at = Some(7); // block 6, in the sector that starts at block 4
        let image = firmware(20_000);
        flash(&mut rom, &image, APP_OFFSET, &mut |_, _| {}).unwrap();
        let at = APP_OFFSET as usize;
        assert_eq!(rom.flash[at..at + 20_000], image[..]);
        assert_eq!(rom.sessions, 2);
    }

    #[test]
    fn refuses_what_does_not_belong() {
        for (image, offset, want) in [
            (vec![0x00, 0x50, 0x00, 0x20, 0x31, 0x01, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0, 0], APP_OFFSET, "not an ESP firmware image"),
            (firmware(4096), 0x1234, "offset must be"),
            ({
                let mut f = firmware(4096);
                f[12] = 9; // an ESP32-S3 image
                f
            }, APP_OFFSET, "another chip"),
        ] {
            let mut rom = SimRom::new();
            let e = flash(&mut rom, &image, offset, &mut |_, _| {}).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
            assert_eq!(rom.sessions, 0, "{want}: nothing may be erased");
        }
    }
}
