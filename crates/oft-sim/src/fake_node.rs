//! A fake Node that speaks the Node link in memory. Drive it with Timer time; it produces
//! the frames a real C5 would, from a [`Scene`]. Used by `oftd --fake` and the tests.

use crate::{NodeClock, Rng, Scene};
use oft_nodelink::msg::{ChipName, Features, Hello, Message, Plan, Pong, SampleFlags, Samples, Status, SyncEdge, MAX_PLAN_SEATS};
use oft_nodelink::{encode_frame, FrameDecoder, MAX_FRAME};

pub struct FakeNode {
    /// What a seat receives when it has no scene of its own.
    pub scene: Scene,
    /// A seat's own scene (its own pilot), by seat number. See [`FakeNode::set_seat_scene`].
    seat_scenes: [Option<Scene>; MAX_PLAN_SEATS],
    pub clock: NodeClock,
    pub chip_id: u64,
    rng: Rng,
    plan: Option<Plan>,
    /// Timer time the next sample is due. Set when a plan arrives.
    next_sample_timer: Option<u64>,
    /// The batch being filled for each seat of the plan, in plan order.
    pending: [Option<Samples>; MAX_PLAN_SEATS],
    /// Which seat of the plan the next sample is taken on.
    hop: usize,
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
            seat_scenes: core::array::from_fn(|_| None),
            clock,
            chip_id: 0xC5_0000_0000 | (seed & 0xFFFF_FFFF),
            rng: Rng::new(seed),
            plan: None,
            next_sample_timer: None,
            pending: [None; MAX_PLAN_SEATS],
            hop: 0,
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
            chip: ChipName::new("simulated"),
        })
    }

    /// Give `seat` its own scene: its own pilot, with its own passes. Seats without one
    /// all receive `scene`. Seats outside the plan's range are ignored.
    pub fn set_seat_scene(&mut self, seat: u8, scene: Scene) {
        if let Some(slot) = self.seat_scenes.get_mut(seat as usize) {
            *slot = Some(scene);
        }
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
                self.pending = [None; MAX_PLAN_SEATS];
                self.hop = 0;
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
                self.pending = [None; MAX_PLAN_SEATS];
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
    ///
    /// With one seat in the plan it is sampled every `scene.dt_us`. With more, the Node hops
    /// as a real one does: one sample per seat in turn, so each of `n` seats is sampled every
    /// `n * dt_us` and reports that as its sample period.
    pub fn advance(&mut self, t_timer: u64) {
        let Some(plan) = self.plan else { return };
        let n = plan.seats().len();
        if n == 0 {
            return;
        }
        let dt = self.scene.dt_us;
        // About 50 ms of samples per batch, whatever the number of seats.
        let per_batch = (50 / n).max(4);
        let Some(mut next) = self.next_sample_timer else { return };
        while next <= t_timer {
            let slot = self.hop % n;
            self.hop = (slot + 1) % n;
            let (seat, _freq) = plan.seats()[slot];
            let t_node = self.clock.node_time(next);
            let scene = self.seat_scenes.get(seat as usize).and_then(|s| s.as_ref()).unwrap_or(&self.scene);
            let (p, f) = scene.sample(t_node, &mut self.rng);
            let floor = (scene.floor_db * 10.0).round() as i16;
            let batch = self.pending[slot].get_or_insert_with(|| Samples::new(seat, t_node, (dt * n as u64) as u16, floor, SampleFlags::empty()));
            batch.flags.0 |= f.0;
            batch.push(p).unwrap();
            if batch.len() >= per_batch {
                let b = self.pending[slot].take().unwrap();
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
