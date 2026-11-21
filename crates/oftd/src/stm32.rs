//! The STM32's built-in serial bootloader (ST application note AN3155): enough of it to
//! read, erase, write and verify a Node's flash over the Node link's own wires.
//!
//! The bootloader lives in ROM, so a failed or wrong write cannot lock a board out: the
//! update can always be run again. Everything here goes through [`Wire`], which the tests
//! implement with a simulated chip.

use std::io;
use std::time::Duration;

const ACK: u8 = 0x79;
const NACK: u8 = 0x1F;
pub const FLASH_BASE: u32 = 0x0800_0000;
/// STM32F1 system memory: the flash size in KiB.
const F1_FLASH_SIZE_ADDR: u32 = 0x1FFF_F7E0;
/// Bootloader baud rate. It measures whatever the host uses; this is what RotorHazard's
/// updater uses on the same boards.
pub const BOOT_BAUD: u32 = 57_600;
/// Bytes per read or write command (the protocol's maximum).
const CHUNK: usize = 256;

/// Product ids this flasher writes to, with their flash page size in bytes.
const PRODUCTS: [(u16, &str, usize); 3] = [
    (0x410, "STM32F103 medium-density", 1024),
    (0x412, "STM32F103 low-density", 1024),
    (0x414, "STM32F103 high-density", 2048),
];

/// The serial line while the chip is in its bootloader (8 data bits, even parity).
pub trait Wire {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// Fill `buf`, or fail once `timeout` has passed.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<()>;
    /// Drop anything received and not yet read.
    fn discard(&mut self);
}

/// What the chip said about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub product_id: u16,
    pub name: &'static str,
    pub page: usize,
    pub flash_bytes: usize,
    commands: Vec<u8>,
}

/// Where a flash run is, for the progress shown to the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Connecting,
    BackingUp,
    Erasing,
    Writing,
    Verifying,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Connecting => "connecting",
            Step::BackingUp => "backing up",
            Step::Erasing => "erasing",
            Step::Writing => "writing",
            Step::Verifying => "verifying",
        }
    }
}

/// Whether `image` starts like firmware linked for the start of an STM32's flash: an
/// initial stack pointer in RAM, then a reset address in flash.
pub fn check_image(image: &[u8]) -> Result<(), String> {
    if image.len() < 8 {
        return Err("the file is too short to be firmware".into());
    }
    let word = |at: usize| u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]]);
    let (sp, reset) = (word(0), word(4));
    if !(0x2000_0000..=0x2002_0000).contains(&sp) || reset >> 24 != 0x08 || reset & 1 == 0 {
        return Err("this is not STM32 firmware (it does not start with a stack pointer and a reset address)".into());
    }
    Ok(())
}

fn xor(data: &[u8]) -> u8 {
    data.iter().fold(0, |x, b| x ^ b)
}

pub struct Bootloader<'a> {
    wire: &'a mut dyn Wire,
}

impl<'a> Bootloader<'a> {
    pub fn new(wire: &'a mut dyn Wire) -> Self {
        Self { wire }
    }

    fn byte(&mut self, what: &str, timeout: Duration) -> Result<u8, String> {
        let mut b = [0u8];
        self.wire.read(&mut b, timeout).map_err(|_| format!("{what}: the bootloader did not answer"))?;
        Ok(b[0])
    }

    fn ack(&mut self, what: &str, timeout: Duration) -> Result<(), String> {
        match self.byte(what, timeout)? {
            ACK => Ok(()),
            NACK => Err(format!("{what}: the bootloader refused")),
            b => Err(format!("{what}: unexpected answer 0x{b:02x}")),
        }
    }

    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.wire.write(data).map_err(|e| format!("could not write to the serial port: {e}"))
    }

    fn fill(&mut self, buf: &mut [u8], what: &str) -> Result<(), String> {
        self.wire.read(buf, Duration::from_secs(2)).map_err(|_| format!("{what}: the answer stopped part way"))
    }

    /// Send the byte the bootloader measures the baud rate from.
    pub fn connect(&mut self) -> Result<(), String> {
        self.wire.discard();
        self.send(&[0x7F])?;
        // NACK: it had already measured the baud rate.
        match self.byte("connect", Duration::from_secs(1))? {
            ACK | NACK => Ok(()),
            b => Err(format!("connect: not a bootloader answer (0x{b:02x})")),
        }
    }

    fn cmd(&mut self, c: u8) -> Result<(), String> {
        self.send(&[c, !c])?;
        self.ack(&format!("command 0x{c:02x}"), Duration::from_secs(1))
    }

    fn addr(&mut self, a: u32) -> Result<(), String> {
        let raw = a.to_be_bytes();
        self.send(&[raw[0], raw[1], raw[2], raw[3], xor(&raw)])?;
        self.ack(&format!("address 0x{a:08x}"), Duration::from_secs(1))
    }

    /// A length byte, then that many plus one data bytes, then ACK.
    fn counted(&mut self, what: &str) -> Result<Vec<u8>, String> {
        let n = self.byte(what, Duration::from_secs(1))? as usize;
        let mut data = vec![0u8; n + 1];
        self.fill(&mut data, what)?;
        self.ack(what, Duration::from_secs(1))?;
        Ok(data)
    }

    pub fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), String> {
        assert!(!buf.is_empty() && buf.len() <= CHUNK);
        self.cmd(0x11)?;
        self.addr(addr)?;
        let n = (buf.len() - 1) as u8;
        self.send(&[n, !n])?;
        self.ack("read length", Duration::from_secs(1))?;
        self.fill(buf, "read")
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), String> {
        assert!(!data.is_empty() && data.len() <= CHUNK && data.len() % 4 == 0);
        self.cmd(0x31)?;
        self.addr(addr)?;
        let mut body = Vec::with_capacity(data.len() + 2);
        body.push((data.len() - 1) as u8);
        body.extend_from_slice(data);
        body.push(xor(&body));
        self.send(&body)?;
        self.ack("write", Duration::from_secs(2))
    }

    fn erase(&mut self, pages: usize, commands: &[u8]) -> Result<(), String> {
        let mut body = Vec::new();
        if commands.contains(&0x43) {
            self.cmd(0x43)?;
            body.push((pages - 1) as u8);
            body.extend((0..pages).map(|p| p as u8));
        } else {
            // The two-byte form newer bootloaders use.
            self.cmd(0x44)?;
            body.extend(((pages - 1) as u16).to_be_bytes());
            for p in 0..pages as u16 {
                body.extend(p.to_be_bytes());
            }
        }
        body.push(xor(&body));
        self.send(&body)?;
        self.ack("erase", Duration::from_secs(30))
    }

    /// Identify the chip; refuses one this flasher does not know.
    pub fn chip(&mut self) -> Result<Chip, String> {
        self.cmd(0x00)?;
        let get = self.counted("get")?;
        let commands = get[1..].to_vec();
        self.cmd(0x02)?;
        let id = self.counted("get id")?;
        let product_id = id.iter().fold(0u16, |v, b| (v << 8) | *b as u16);
        let (_, name, page) = *PRODUCTS
            .iter()
            .find(|p| p.0 == product_id)
            .ok_or(format!("the Node's chip (product id 0x{product_id:03x}) is not one this Timer can update"))?;
        let mut kib = [0u8; 2];
        self.read(F1_FLASH_SIZE_ADDR, &mut kib).map_err(|e| format!("{e} (the chip may be read-protected)"))?;
        Ok(Chip { product_id, name, page, flash_bytes: u16::from_le_bytes(kib) as usize * 1024, commands })
    }
}

/// Write `image` to a chip already restarted into its bootloader, and check it reads back.
/// With `backup`, the whole flash is read into it first. `progress` gets the step and the
/// bytes done in it.
pub fn flash(wire: &mut dyn Wire, image: &[u8], mut backup: Option<&mut Vec<u8>>, progress: &mut dyn FnMut(Step, usize)) -> Result<Chip, String> {
    check_image(image)?;
    let mut image = image.to_vec();
    image.resize(image.len().div_ceil(4) * 4, 0xFF);

    progress(Step::Connecting, 0);
    let mut bl = Bootloader::new(wire);
    bl.connect()?;
    let chip = bl.chip()?;
    if image.len() > chip.flash_bytes {
        return Err(format!("the firmware ({} bytes) does not fit the Node's {} KiB of flash", image.len(), chip.flash_bytes / 1024));
    }

    if let Some(out) = backup.as_deref_mut() {
        out.clear();
        while out.len() < chip.flash_bytes {
            let n = CHUNK.min(chip.flash_bytes - out.len());
            let at = out.len();
            out.resize(at + n, 0);
            bl.read(FLASH_BASE + at as u32, &mut out[at..])?;
            progress(Step::BackingUp, out.len());
        }
    }

    progress(Step::Erasing, 0);
    bl.erase(image.len().div_ceil(chip.page), &chip.commands)?;
    for (k, chunk) in image.chunks(CHUNK).enumerate() {
        bl.write(FLASH_BASE + (k * CHUNK) as u32, chunk)?;
        progress(Step::Writing, k * CHUNK + chunk.len());
    }
    let mut got = [0u8; CHUNK];
    for (k, chunk) in image.chunks(CHUNK).enumerate() {
        bl.read(FLASH_BASE + (k * CHUNK) as u32, &mut got[..chunk.len()])?;
        if got[..chunk.len()] != *chunk {
            return Err(format!("what was written does not read back at byte {}", k * CHUNK));
        }
        progress(Step::Verifying, k * CHUNK + chunk.len());
    }
    Ok(chip)
}

/// A simulated STM32F103C8 bootloader, for tests.
pub mod sim {
    use super::*;
    use std::collections::VecDeque;

    pub struct SimChip {
        pub flash: Vec<u8>,
        pub product_id: u16,
        /// Corrupt one written byte, to test the verify step.
        pub flip_at: Option<usize>,
        /// Refuse reads, as a read-protected chip does.
        pub protected: bool,
        rx: Vec<u8>,
        tx: VecDeque<u8>,
        connected: bool,
    }

    impl SimChip {
        pub fn new(kib: usize) -> Self {
            Self { flash: vec![0xAB; kib * 1024], product_id: 0x410, flip_at: None, protected: false, rx: Vec::new(), tx: VecDeque::new(), connected: false }
        }

        fn mem(&self, addr: u32, n: usize) -> Option<Vec<u8>> {
            if addr == F1_FLASH_SIZE_ADDR && n == 2 {
                return Some(((self.flash.len() / 1024) as u16).to_le_bytes().to_vec());
            }
            let at = addr.checked_sub(FLASH_BASE)? as usize;
            self.flash.get(at..at + n).map(|s| s.to_vec())
        }

        /// Act on the bytes received so far if they make a whole exchange.
        fn step(&mut self) {
            if !self.connected {
                if self.rx.first() == Some(&0x7F) {
                    self.connected = true;
                    self.rx.remove(0);
                    self.tx.push_back(ACK);
                }
                return;
            }
            // Called after every byte: answer at the points the real bootloader does.
            let r = self.rx.clone();
            let n = r.len();
            if n < 2 {
                return;
            }
            if n == 2 {
                let known = [0x00, 0x02, 0x11, 0x31, 0x43].contains(&r[0]);
                if r[1] != !r[0] || !known || (r[0] == 0x11 && self.protected) {
                    self.tx.push_back(NACK);
                    self.rx.clear();
                    return;
                }
                self.tx.push_back(ACK);
                match r[0] {
                    0x00 => self.tx.extend([5, 0x22, 0x00, 0x02, 0x11, 0x31, 0x43, ACK]),
                    0x02 => self.tx.extend([1, (self.product_id >> 8) as u8, self.product_id as u8, ACK]),
                    _ => return,
                }
                self.rx.clear();
                return;
            }
            let addr = || u32::from_be_bytes([r[2], r[3], r[4], r[5]]);
            let done = match r[0] {
                0x11 | 0x31 if n == 7 => {
                    self.tx.push_back(ACK);
                    false
                }
                0x11 if n == 9 => {
                    match self.mem(addr(), r[7] as usize + 1) {
                        Some(d) => {
                            self.tx.push_back(ACK);
                            self.tx.extend(d);
                        }
                        None => self.tx.push_back(NACK),
                    }
                    true
                }
                0x31 if n >= 8 && n == 8 + r[7] as usize + 2 => {
                    let at = (addr() - FLASH_BASE) as usize;
                    for (k, b) in r[8..n - 1].iter().enumerate() {
                        // Flash only clears bits until it is erased.
                        self.flash[at + k] &= if self.flip_at == Some(at + k) { !b } else { *b };
                    }
                    self.tx.push_back(ACK);
                    true
                }
                0x43 if n == 3 + r[2] as usize + 2 => {
                    for p in &r[3..n - 1] {
                        self.flash[*p as usize * 1024..(*p as usize + 1) * 1024].fill(0xFF);
                    }
                    self.tx.push_back(ACK);
                    true
                }
                _ => false,
            };
            if done {
                self.rx.clear();
            }
        }
    }

    impl Wire for SimChip {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            for b in data {
                self.rx.push(*b);
                self.step();
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
    use super::sim::SimChip;
    use super::*;

    fn firmware(len: usize) -> Vec<u8> {
        let mut f: Vec<u8> = (0..len).map(|k| (k * 7 + 3) as u8).collect();
        f[..4].copy_from_slice(&0x2000_5000u32.to_le_bytes());
        f[4..8].copy_from_slice(&0x0800_0131u32.to_le_bytes());
        f
    }

    #[test]
    fn writes_verifies_and_backs_up() {
        let mut chip = SimChip::new(64);
        let before = chip.flash.clone();
        let image = firmware(5001); // not a multiple of four: padded with 0xFF
        let (mut backup, mut steps) = (Vec::new(), Vec::new());
        let got = flash(&mut chip, &image, Some(&mut backup), &mut |s, _| {
            if steps.last() != Some(&s) {
                steps.push(s)
            }
        })
        .unwrap();
        assert_eq!((got.product_id, got.flash_bytes, got.page), (0x410, 65536, 1024));
        assert_eq!(backup, before);
        assert_eq!(chip.flash[..5001], image[..]);
        assert_eq!(chip.flash[5001..5004], [0xFF; 3]);
        assert_eq!(chip.flash[5004..5120], [0xFF; 116], "the rest of the last erased page");
        assert_eq!(chip.flash[5120..], before[5120..], "pages past the image are left alone");
        assert_eq!(steps, [Step::Connecting, Step::BackingUp, Step::Erasing, Step::Writing, Step::Verifying]);
    }

    #[test]
    fn a_bad_write_is_caught_by_the_verify() {
        let mut chip = SimChip::new(64);
        chip.flip_at = Some(1234);
        let e = flash(&mut chip, &firmware(4096), None, &mut |_, _| {}).unwrap_err();
        assert!(e.contains("does not read back at byte 1024"), "{e}");
    }

    #[test]
    fn refuses_before_erasing() {
        let cases: [(&str, Box<dyn Fn(&mut SimChip) -> Vec<u8>>); 4] = [
            ("not STM32 firmware", Box::new(|_| vec![0xE9; 4096])), // an ESP32 image
            ("does not fit", Box::new(|_| firmware(70_000))),
            ("not one this Timer can update", Box::new(|c| {
                c.product_id = 0x999;
                firmware(4096)
            })),
            ("read-protected", Box::new(|c| {
                c.protected = true;
                firmware(4096)
            })),
        ];
        for (want, setup) in cases {
            let mut chip = SimChip::new(64);
            let image = setup(&mut chip);
            let before = chip.flash.clone();
            let e = flash(&mut chip, &image, None, &mut |_, _| {}).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
            assert_eq!(chip.flash, before, "{want}: nothing may be erased");
        }
    }

    #[test]
    fn a_silent_chip_is_reported() {
        struct Dead;
        impl Wire for Dead {
            fn write(&mut self, _: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn read(&mut self, _: &mut [u8], _: Duration) -> io::Result<()> {
                Err(io::ErrorKind::TimedOut.into())
            }
            fn discard(&mut self) {}
        }
        let e = flash(&mut Dead, &firmware(4096), None, &mut |_, _| {}).unwrap_err();
        assert!(e.contains("did not answer"), "{e}");
    }
}
