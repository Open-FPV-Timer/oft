//! Updating the Node's firmware from the Timer (`/fw/node`), for a Node that is an STM32
//! on the Pi's own UART with its reset and BOOT0 lines on two GPIOs (RotorHazard-style
//! boards such as the NuclearHazard). The Timer stops talking to the Node, restarts it into
//! its ROM bootloader, writes and checks the file, and restarts it into the new firmware.

use crate::stm32::{self, Step, Wire};
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

impl Stm32Pins {
    /// Through Raspberry Pi OS's `pinctrl`, which needs no root for a user in `gpio`.
    fn set(pin: u8, mode: &[&str]) -> Result<(), String> {
        let out = Command::new("pinctrl").arg("set").arg(pin.to_string()).args(mode).output().map_err(|e| format!("cannot run pinctrl: {e}"))?;
        if !out.status.success() {
            return Err(format!("pinctrl could not set GPIO{pin}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }

    /// Restart the STM32, into its bootloader or its firmware.
    pub fn restart(&self, bootloader: bool) -> Result<(), String> {
        Self::set(self.boot0, if bootloader { &["op", "dh"] } else { &["op", "dl"] })?;
        Self::set(self.reset, &["op", "dl"])?;
        std::thread::sleep(Duration::from_millis(50));
        Self::set(self.reset, &["ip", "pn"])?; // released: the STM32 pulls its own reset line up
        std::thread::sleep(Duration::from_millis(100));
        if !bootloader {
            Self::set(self.boot0, &["ip", "pd"])?;
        }
        Ok(())
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

/// Start an update with `image`. Returns the state to answer with, or why it cannot start.
pub fn start(timer: Arc<Timer>, image: Vec<u8>) -> Result<NodeFw, String> {
    let pins = timer.cfg.stm32.ok_or("this Timer cannot update its Node")?;
    stm32::check_image(&image)?;
    {
        let mut st = timer.node_fw.lock().unwrap();
        if st.running {
            return Err("a Node update is already running".into());
        }
        *st = NodeFw { running: true, step: Step::Connecting.name(), bytes: image.len(), ..NodeFw::default() };
    }
    let state = timer.node_fw.lock().unwrap().clone();
    std::thread::Builder::new().name("oft-node-fw".into()).spawn(move || run(timer, pins, image)).map_err(|e| {
        format!("could not start the update: {e}")
    })?;
    Ok(state)
}

fn run(timer: Arc<Timer>, pins: Stm32Pins, image: Vec<u8>) {
    let began = Instant::now();
    // Firmware that has introduced itself is ours and can be built again; anything else
    // (the board's original firmware) is read out and kept before it is erased.
    let known = timer.hellos.load(Ordering::Relaxed) > 0;
    let backup_dir = timer.cfg.settings.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut backup = (!known && backup_dir.is_some()).then(Vec::new);
    let mut backup_path = None;

    // The driver loop leaves the link alone while this is set; give it a tick to notice.
    timer.flashing.store(true, Ordering::SeqCst);
    std::thread::sleep(timer.cfg.tick * 3);
    let result = {
        let mut link = timer.link.lock().unwrap();
        let r = (|| {
            link.bootloader_mode(true).map_err(|e| format!("could not switch the serial port: {e}"))?;
            pins.restart(true)?;
            let t = &timer;
            let chip = stm32::flash(&mut LinkWire(&mut **link), &image, backup.as_mut(), &mut |step, done| {
                let mut st = t.node_fw.lock().unwrap();
                st.step = step.name();
                st.written = done;
            })?;
            Ok::<_, String>(chip)
        })();
        // Whatever happened, leave the STM32 running firmware and the port as it was.
        let restarted = pins.restart(false);
        let _ = link.bootloader_mode(false);
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
        Ok(chip) => eprintln!("oftd: Node updated ({} bytes to a {})", image.len(), chip.name),
        Err(e) => eprintln!("oftd: Node update failed: {e}"),
    }
    timer.node_fw.lock().unwrap().step = "restarting";
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && timer.hellos.load(Ordering::Relaxed) == before {
        std::thread::sleep(Duration::from_millis(100));
    }
    finish(timer.hellos.load(Ordering::Relaxed) != before);
}
