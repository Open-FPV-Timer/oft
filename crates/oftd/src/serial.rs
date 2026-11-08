//! A real Node on a serial port (the XIAO C5's USB-C, or a USB-UART adapter). Reads happen
//! on a blocking thread and are handed over through a channel so `Timer::run`'s loop stays
//! non-blocking. No SYNC wire on a serial link: the clock comes from ping fit.

use crate::link::NodeLink;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
    rx: Receiver<Vec<u8>>,
    path: String,
    pub read_errors: u32,
}

impl SerialLink {
    pub fn open(path: &str, baud: u32) -> Result<Self, serialport::Error> {
        let port = serialport::new(path, baud).timeout(Duration::from_millis(50)).open()?;
        let mut reader = port.try_clone()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("oft-serial-rx".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
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
        Ok(Self { port, rx, path: path.to_string(), read_errors: 0 })
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
}
