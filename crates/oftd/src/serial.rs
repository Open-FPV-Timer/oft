//! A real Node on a serial port (the XIAO C5's USB-C, or a USB-UART adapter). Reads happen
//! on a blocking thread and are handed over through a channel so `Timer::run`'s loop stays
//! non-blocking. No SYNC wire on a serial link: the clock comes from ping fit.

use crate::link::{BootLine, NodeLink};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
    rx: Receiver<Vec<u8>>,
    path: String,
    baud: u32,
    /// Set while a firmware update has the port: the reader thread stays off it.
    paused: Arc<AtomicBool>,
    pub read_errors: u32,
}

impl SerialLink {
    pub fn open(path: &str, baud: u32) -> Result<Self, serialport::Error> {
        let port = serialport::new(path, baud).timeout(Duration::from_millis(50)).open()?;
        let mut reader = port.try_clone()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let paused = Arc::new(AtomicBool::new(false));
        let reader_paused = paused.clone();
        thread::Builder::new()
            .name("oft-serial-rx".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    if reader_paused.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            // Unplugged: back off and keep trying so a replug recovers.
                            thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            })
            .expect("spawn serial reader");
        Ok(Self { port, rx, path: path.to_string(), baud, paused, read_errors: 0 })
    }
}

impl NodeLink for SerialLink {
    fn write(&mut self, bytes: &[u8], _t_timer: u64) {
        let _ = self.port.write_all(bytes);
    }

    fn read(&mut self, _t_timer: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.read_errors = self.read_errors.wrapping_add(1);
                    break;
                }
            }
        }
        out
    }

    fn sync_pulse(&mut self, _t_timer: u64) {}

    fn has_sync(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!("serial:{}", self.path)
    }

    fn bootloader_mode(&mut self, line: Option<BootLine>) -> std::io::Result<()> {
        if let Some(line) = line {
            self.paused.store(true, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(150)); // the reader's read in progress times out
            while self.rx.try_recv().is_ok() {}
            self.port.set_baud_rate(line.baud)?;
            self.port.set_parity(if line.even_parity { serialport::Parity::Even } else { serialport::Parity::None })?;
        } else {
            // Put back as much as possible even if one step fails.
            let parity = self.port.set_parity(serialport::Parity::None);
            let baud = self.port.set_baud_rate(self.baud);
            let _ = self.port.set_timeout(Duration::from_millis(50));
            let _ = self.port.clear(serialport::ClearBuffer::All);
            self.paused.store(false, Ordering::Relaxed);
            parity?;
            baud?;
        }
        Ok(())
    }

    fn raw_write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.port.write_all(data)?;
        self.port.flush()
    }

    fn raw_read(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<()> {
        let end = Instant::now() + timeout;
        let mut got = 0;
        while got < buf.len() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.port.set_timeout(left)?;
            match self.port.read(&mut buf[got..]) {
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn raw_discard(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::All);
    }
}
