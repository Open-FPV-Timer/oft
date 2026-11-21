//! Node transports. Bytes in, bytes out, plus the SYNC pulse the Timer drives.

use oft_sim::fake_node::FakeNode;

pub trait NodeLink: Send {
    /// Bytes the Timer sends to the Node. `t_timer` is now, µs.
    fn write(&mut self, bytes: &[u8], t_timer: u64);
    /// Bytes the Node has produced since the last read.
    fn read(&mut self, t_timer: u64) -> Vec<u8>;
    /// The Timer pulsed SYNC at `t_timer`. Real hardware toggles a GPIO here.
    fn sync_pulse(&mut self, t_timer: u64);
    /// Whether this transport can carry a SYNC pulse at all.
    fn has_sync(&self) -> bool;
    fn describe(&self) -> String;

    /// For a firmware update over the same wires (`nodefw.rs`): stop reading the Node link
    /// and set the line up for the chip's bootloader (`Some`), or put it back (`None`). The
    /// `raw_*` calls work in between. Only a real serial port can do this.
    fn bootloader_mode(&mut self, _line: Option<BootLine>) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn raw_write(&mut self, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    /// Fill `buf`, or fail once `timeout` has passed.
    fn raw_read(&mut self, _buf: &mut [u8], _timeout: std::time::Duration) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn raw_discard(&mut self) {}
}

/// How a chip's bootloader wants the serial line: 8 data bits, one stop bit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BootLine {
    pub baud: u32,
    pub even_parity: bool,
}

/// The simulator, in process. `read` advances the scene to `t_timer` first.
pub struct FakeLink(pub FakeNode);

impl NodeLink for FakeLink {
    fn write(&mut self, bytes: &[u8], t_timer: u64) {
        self.0.write(bytes, t_timer);
    }
    fn read(&mut self, t_timer: u64) -> Vec<u8> {
        self.0.advance(t_timer);
        self.0.take_output()
    }
    fn sync_pulse(&mut self, t_timer: u64) {
        self.0.sync_edge(t_timer);
    }
    fn has_sync(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "fake".into()
    }
}
