//! Updating the Node's firmware from the Timer (`/fw/node`), for a Node on the Pi's own
//! UART with its reset and boot-select lines on two GPIOs: an STM32 (RotorHazard-style
//! boards such as the NuclearHazard) or an ESP32-C5. The Timer stops talking to the Node,
//! restarts it into its ROM bootloader, writes and checks the file, and restarts it into
//! the new firmware.

use crate::link::BootLine;
use crate::stm32::{self, Wire};
use crate::{esp, stm32::Step};
use crate::timer::Timer;
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The GPIOs (BCM numbers) wired to the STM32's reset and BOOT0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stm32Pins {
    pub reset: u8,
    pub boot0: u8,
}

/// The GPIOs (BCM numbers) wired to an ESP32's EN (reset) and BOOT pins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EspPins {
    pub en: u8,
    pub boot: u8,
}

/// The kind of Node this Timer can update, and the pins that restart it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeFlasher {
    Stm32(Stm32Pins),
    Esp(EspPins),
}

/// Through Raspberry Pi OS's `pinctrl`, which needs no root for a user in `gpio`.
fn set_pin(pin: u8, mode: &[&str]) -> Result<(), String> {
    let out = Command::new("pinctrl").arg("set").arg(pin.to_string()).args(mode).output().map_err(|e| format!("cannot run pinctrl: {e}"))?;
    if !out.status.success() {
        return Err(format!("pinctrl could not set GPIO{pin}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

impl NodeFlasher {
    /// Restart the Node, into its bootloader or its firmware.
    pub fn restart(&self, bootloader: bool) -> Result<(), String> {
        match *self {
            NodeFlasher::Stm32(p) => {
                set_pin(p.boot0, if bootloader { &["op", "dh"] } else { &["op", "dl"] })?;
                set_pin(p.reset, &["op", "dl"])?;
                std::thread::sleep(Duration::from_millis(50));
                set_pin(p.reset, &["ip", "pn"])?; // released: the STM32 pulls its own reset line up
                std::thread::sleep(Duration::from_millis(100));
                if !bootloader {
                    set_pin(p.boot0, &["ip", "pd"])?;
                }
            }
            // As the RP2350 Timer does it: BOOT held low through a reset means download
            // mode. Both lines are released afterwards; the board pulls them up itself.
            NodeFlasher::Esp(p) => {
                set_pin(p.boot, if bootloader { &["op", "dl"] } else { &["op", "dh"] })?;
                set_pin(p.en, &["op", "dl"])?;
                std::thread::sleep(Duration::from_millis(20));
                set_pin(p.en, &["op", "dh"])?;
                std::thread::sleep(Duration::from_millis(50));
                set_pin(p.en, &["ip", "pn"])?;
                set_pin(p.boot, &["ip", "pn"])?;
            }
        }
        Ok(())
    }

    fn line(&self) -> BootLine {
        match self {
            NodeFlasher::Stm32(_) => BootLine { baud: stm32::BOOT_BAUD, even_parity: true },
            NodeFlasher::Esp(_) => BootLine { baud: esp::ROM_BAUD, even_parity: false },
        }
    }
}

/// `GET /fw/node`: the update in progress, or the last one's result.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NodeFw {
    pub running: bool,
    pub step: &'static str,
    /// Bytes done in the current step.
    pub written: usize,
    /// Size of the firmware.
    pub bytes: usize,
    pub done: bool,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub seconds: u64,
    /// The Node introduced itself again afterwards.
    pub returned: bool,
    /// Where the chip's previous contents were saved, when they were.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

struct LinkWire<'a>(&'a mut dyn crate::link::NodeLink);

impl Wire for LinkWire<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.0.raw_write(data)
    }
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<()> {
        self.0.raw_read(buf, timeout)
    }
    fn discard(&mut self) {
        self.0.raw_discard()
    }
}

/// Save the chip's previous contents next to the settings, named by what they are so the
/// same contents are only kept once.
fn save_backup(dir: &std::path::Path, data: &[u8]) -> Result<PathBuf, String> {
    let used = data.iter().rposition(|b| *b != 0xFF).map_or(0, |p| p + 1);
    let hash = data.iter().fold(0x811c_9dc5u32, |h, b| (h ^ *b as u32).wrapping_mul(0x0100_0193));
    let path = dir.join(format!("node-backup-{hash:08x}.bin"));
    if used > 0 && !path.exists() {
        std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, data)).map_err(|e| format!("could not save a backup to {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// Start an update with `image`. `offset` is where in an ESP32's flash it goes (the
/// firmware's place if not given); an STM32 has only the one place. Returns the state to
/// answer with, or why it cannot start.
pub fn start(timer: Arc<Timer>, image: Vec<u8>, offset: Option<u32>) -> Result<NodeFw, String> {
    let pins = timer.cfg.node_fw.ok_or("this Timer cannot update its Node")?;
    let offset = offset.unwrap_or(esp::APP_OFFSET);
    match pins {
        NodeFlasher::Stm32(_) => stm32::check_image(&image)?,
        NodeFlasher::Esp(_) => esp::check_image(offset, &image)?,
    }
    {
        let mut st = timer.node_fw.lock().unwrap();
        if st.running {
            return Err("a Node update is already running".into());
        }
        *st = NodeFw { running: true, step: Step::Connecting.name(), bytes: image.len(), ..NodeFw::default() };
    }
    let state = timer.node_fw.lock().unwrap().clone();
    std::thread::Builder::new().name("oft-node-fw".into()).spawn(move || run(timer, pins, image, offset)).map_err(|e| {
        format!("could not start the update: {e}")
    })?;
    Ok(state)
}

fn run(timer: Arc<Timer>, pins: NodeFlasher, image: Vec<u8>, offset: u32) {
    let began = Instant::now();
    // Firmware that has introduced itself is ours and can be built again; anything else
    // (the board's original firmware) is read out and kept before it is erased.
    let known = timer.hellos.load(Ordering::Relaxed) > 0;
    let backup_dir = timer.cfg.settings.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut backup = (!known && backup_dir.is_some() && matches!(pins, NodeFlasher::Stm32(_))).then(Vec::new);
    let mut backup_path = None;

    // The driver loop leaves the link alone while this is set; give it a tick to notice.
    timer.flashing.store(true, Ordering::SeqCst);
    std::thread::sleep(timer.cfg.tick * 3);
    let result = {
        let mut link = timer.link.lock().unwrap();
        let r = (|| {
            link.bootloader_mode(Some(pins.line())).map_err(|e| format!("could not switch the serial port: {e}"))?;
            pins.restart(true)?;
            let t = &timer;
            let mut progress = |step: &'static str, done: usize| {
                let mut st = t.node_fw.lock().unwrap();
                st.step = step;
                st.written = done;
            };
            let mut wire = LinkWire(&mut **link);
            match pins {
                NodeFlasher::Stm32(_) => stm32::flash(&mut wire, &image, backup.as_mut(), &mut |step: Step, done| progress(step.name(), done)).map(|chip| chip.name),
                NodeFlasher::Esp(_) => esp::flash(&mut wire, &image, offset, &mut progress).map(|_| "ESP32-C5"),
            }
        })();
        // Whatever happened, leave the STM32 running firmware and the port as it was.
        let restarted = pins.restart(false);
        let _ = link.bootloader_mode(None);
        r.and_then(|chip| restarted.map(|_| chip))
    };
    if let (Some(data), Some(dir)) = (backup.as_ref().filter(|d| !d.is_empty()), backup_dir.as_ref()) {
        // A failed save does not fail the update: the flash has already been written.
        match save_backup(dir, data) {
            Ok(p) => backup_path = Some(p.display().to_string()),
            Err(e) => eprintln!("oftd: {e}"),
        }
    }
    let before = timer.hellos.load(Ordering::Relaxed);
    timer.flashing.store(false, Ordering::SeqCst);

    let finish = |returned: bool| {
        let mut st = timer.node_fw.lock().unwrap();
        st.running = false;
        st.done = true;
        st.ok = result.is_ok();
        st.error = result.as_ref().err().cloned();
        st.seconds = began.elapsed().as_secs();
        st.returned = returned;
        st.backup = backup_path.clone();
    };
    match &result {
        Ok(chip) => eprintln!("oftd: Node updated ({} bytes to a {chip})", image.len()),
        Err(e) => eprintln!("oftd: Node update failed: {e}"),
    }
    timer.node_fw.lock().unwrap().step = "restarting";
    // A C5 calibrates its radio before it speaks, longer on its first start.
    let deadline = Instant::now() + Duration::from_secs(if matches!(pins, NodeFlasher::Esp(_)) { 40 } else { 10 });
    while Instant::now() < deadline && timer.hellos.load(Ordering::Relaxed) == before {
        std::thread::sleep(Duration::from_millis(100));
    }
    finish(timer.hellos.load(Ordering::Relaxed) != before);
}
