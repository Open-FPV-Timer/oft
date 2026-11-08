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
