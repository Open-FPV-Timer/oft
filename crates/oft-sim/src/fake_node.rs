//! A fake Node that speaks the Node link in memory. Drive it with Timer time; it produces
//! the frames a real C5 would, from a [`Scene`]. Used by `oftd --fake` and the tests.

use crate::{NodeClock, Rng, Scene};
use oft_nodelink::msg::{Features, Hello, Message, Plan, Pong, SampleFlags, Samples, Status, SyncEdge};
use oft_nodelink::{encode_frame, FrameDecoder, MAX_FRAME};

pub struct FakeNode {
    pub scene: Scene,
    pub clock: NodeClock,
    pub chip_id: u64,
    rng: Rng,
    plan: Option<Plan>,
    /// Timer time the next sample is due. Set when a plan arrives.
    next_sample_timer: Option<u64>,
    /// Node time of the sample that opens the pending batch, and its powers.
    pending: Option<Samples>,
    per_batch: usize,
    edge_seq: u32,
    decoder: FrameDecoder,
    /// Encoded frames waiting for the Timer to read.
    out: Vec<u8>,
    identify_until: Option<u64>,
    pub resets: u32,
    pub dfu_requests: u32,
}

impl FakeNode {
    pub fn new(scene: Scene, clock: NodeClock, seed: u64) -> Self {
        let mut n = Self {
            scene,
            clock,
            chip_id: 0xC5_0000_0000 | (seed & 0xFFFF_FFFF),
            rng: Rng::new(seed),
            plan: None,
            next_sample_timer: None,
            pending: None,
            per_batch: 50,
            edge_seq: 0,
            decoder: FrameDecoder::new(),
            out: Vec::new(),
            identify_until: None,
            resets: 0,
            dfu_requests: 0,
        };
        n.send(&n.hello());
        n
    }

    pub fn hello(&self) -> Message {
        Message::Hello(Hello {
            chip_id: self.chip_id,
            fw: [0, 1, 0],
            link_version: oft_nodelink::LINK_VERSION,
            freq_min_mhz: 5645,
            freq_max_mhz: 5945,
            max_seats: 8,
            features: Features::SYNC | Features::DFU,
        })
    }

    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    pub fn identifying_at(&self, t_timer: u64) -> bool {
        self.identify_until.is_some_and(|u| t_timer < u)
    }

    fn send(&mut self, m: &Message) {
        let mut buf = [0u8; MAX_FRAME];
        let n = encode_frame(m, &mut buf).expect("fake node frame");
        self.out.extend_from_slice(&buf[..n]);
    }

    /// Bytes the Node has written to the link since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    /// Bytes arriving from the Timer. `t_timer` is when they arrive (for pong timing).
    pub fn write(&mut self, bytes: &[u8], t_timer: u64) {
        for &b in bytes {
            if let Some(Ok(m)) = self.decoder.push(b) {
                self.handle(m, t_timer);
            }
        }
    }

    fn handle(&mut self, m: Message, t_timer: u64) {
        match m {
            Message::Plan(p) => {
                self.plan = Some(p);
                self.pending = None;
                self.next_sample_timer = Some(t_timer);
            }
            Message::Ping(p) => {
                // Respond ~150 µs later in Node time, like firmware would.
                let t_node = self.clock.node_time(t_timer + 150);
                self.send(&Message::Pong(Pong { id: p.id, t_timer: p.t_timer, t_node }));
            }
            Message::Reset => {
                self.resets += 1;
                self.plan = None;
                self.pending = None;
                self.next_sample_timer = None;
                let h = self.hello();
                self.send(&h);
            }
            Message::Identify { duration_ms } => self.identify_until = Some(t_timer + duration_ms as u64 * 1000),
            Message::Dfu => self.dfu_requests += 1,
            // Node → Timer messages arriving at the Node are ignored.
            _ => {}
        }
    }

    /// The Timer pulsed SYNC at `t_timer`. Firmware latches it within ~2 µs.
    pub fn sync_edge(&mut self, t_timer: u64) {
        self.edge_seq += 1;
        let t_node = self.clock.node_time(t_timer) + (self.rng.uniform() * 2.0) as u64;
        self.send(&Message::SyncEdge(SyncEdge { edge_seq: self.edge_seq, t_node }));
    }

    /// Advance to Timer time `t_timer`, emitting sample batches that are due.
    /// Only the first seat in the plan is sampled (single-pilot fake; hopping is TODO).
    pub fn advance(&mut self, t_timer: u64) {
        let Some(plan) = self.plan else { return };
        let Some(&(seat, _freq)) = plan.seats().first() else { return };
        let dt = self.scene.dt_us;
        let Some(mut next) = self.next_sample_timer else { return };
        while next <= t_timer {
            let t_node = self.clock.node_time(next);
            let (p, f) = self.scene.sample(t_node, &mut self.rng);
            let floor = (self.scene.floor_db * 10.0).round() as i16;
            let batch = self.pending.get_or_insert_with(|| Samples::new(seat, t_node, dt as u16, floor, SampleFlags::empty()));
            batch.flags.0 |= f.0;
            batch.push(p).unwrap();
            if batch.len() >= self.per_batch {
                let b = self.pending.take().unwrap();
                self.send(&Message::Samples(b));
            }
            next += dt;
        }
        self.next_sample_timer = Some(next);
    }

    pub fn status(&mut self) {
        self.send(&Message::Status(Status { temp_dc: 412, hop_errors: 0, overruns: 0, gain_index: 60 }));
    }
}
