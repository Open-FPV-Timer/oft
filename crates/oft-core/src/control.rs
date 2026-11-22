//! Control: which client may change the Timer (spec §6.6).
//!
//! Clients that mean to write make a claim; at most one standing claim holds control, and
//! while one does, a write must carry its key. The hosts keep one [`Control`], tie each
//! claim to a client's stream, and check writes with [`Control::allows`].
//!
//! A holder whose stream drops keeps control for [`GRACE_US`], and gets its claim back by
//! claiming again with its key, so a short network drop does not hand control elsewhere.

/// How long the holder's claim outlives its stream, µs.
pub const GRACE_US: u64 = 10_000_000;

/// The refusal for a request to choose the writer that did not come from the Timer's page.
pub const OWN_PAGE_ONLY: &str = "only the Timer's own page may choose who has control";

/// Whether an `Origin` header (`http://192.168.1.50:7270`) names the same host and port as
/// a `Host` header (`192.168.1.50:7270`), ignoring case.
pub fn same_origin(origin: &str, host: &str) -> bool {
    let origin = origin.trim();
    let rest = ["http://", "https://"].iter().find_map(|scheme| {
        let (head, tail) = origin.split_at_checked(scheme.len())?;
        head.eq_ignore_ascii_case(scheme).then_some(tail)
    });
    rest.is_some_and(|o| !o.is_empty() && o.eq_ignore_ascii_case(host.trim()))
}

/// Longest claim name, in bytes.
pub const NAME_MAX: usize = 32;

/// A claim's name for people to read: printable ASCII without `"` and `\`, so it can be
/// put in JSON as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Name {
    len: u8,
    bytes: [u8; NAME_MAX],
}

impl Name {
    /// `text` with every other character dropped, cut to [`NAME_MAX`]; "unnamed" if
    /// nothing is left.
    pub fn new(text: &str) -> Self {
        let mut n = Name { len: 0, bytes: [0; NAME_MAX] };
        for b in text.trim().bytes().filter(|b| (0x20..0x7f).contains(b) && *b != b'"' && *b != b'\\') {
            if n.len as usize == NAME_MAX {
                break;
            }
            n.bytes[n.len as usize] = b;
            n.len += 1;
        }
        if n.len == 0 {
            return Name::new("unnamed");
        }
        n
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    /// Not reused while the Timer runs.
    pub id: u32,
    pub name: Name,
    /// What the holder's writes must carry, as eight hex digits ([`Claim::key_text`]).
    pub key: u32,
    /// Which stream has the claim: a number that changes each time one takes it up.
    pub owner: u32,
    /// Set while no stream has the claim: when it ends, on the caller's µs clock.
    pub away_until: Option<u64>,
}

impl Claim {
    pub fn key_text(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        for (k, o) in out.iter_mut().enumerate() {
            *o = b"0123456789abcdef"[((self.key >> (28 - 4 * k)) & 0xf) as usize];
        }
        out
    }
}

/// Why a request could not be done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// All `N` claims are taken.
    Full,
    /// No standing claim has that id.
    NoSuchClaim,
}

/// Up to `N` standing claims, oldest first, and which of them holds control.
#[derive(Debug, Clone)]
pub struct Control<const N: usize> {
    claims: [Option<Claim>; N],
    holder: Option<u32>,
    next_id: u32,
    seed: u32,
}

impl<const N: usize> Control<N> {
    /// `seed`: anything that differs between starts (the boot id), for the keys.
    pub fn new(seed: u32) -> Self {
        Control { claims: [None; N], holder: None, next_id: 1, seed: seed | 1 }
    }

    pub fn claims(&self) -> impl Iterator<Item = &Claim> {
        self.claims.iter().flatten()
    }

    pub fn get(&self, id: u32) -> Option<&Claim> {
        self.claims().find(|c| c.id == id)
    }

    /// The claim that holds control.
    pub fn holder(&self) -> Option<&Claim> {
        self.holder.and_then(|id| self.get(id))
    }

    /// Make a claim for a stream, or rename the one it already has (`mine`, as returned
    /// here). With the `key` of a standing claim, the stream takes that claim up instead,
    /// from whichever stream had it: a holder coming back after its connection dropped. A
    /// new claim gets control if nobody holds it, and waits otherwise; only [`grant`]
    /// moves control between standing claims. `entropy`: anything that varies, e.g. the
    /// time. Returns the claim's id and owner number.
    ///
    /// [`grant`]: Control::grant
    pub fn claim(&mut self, mine: Option<(u32, u32)>, name: &str, key: Option<&str>, entropy: u32) -> Result<(u32, u32), Error> {
        let name = Name::new(name);
        let owned = mine.and_then(|(id, owner)| self.get(id).filter(|c| c.owner == owner).map(|c| c.id));
        let resumed = key.and_then(|k| self.claims().find(|c| k.trim().as_bytes().eq_ignore_ascii_case(&c.key_text())).map(|c| c.id));
        if let (Some(old), Some(new)) = (owned, resumed) {
            if old != new {
                self.release(old);
            }
        }
        let id = match resumed.or(owned) {
            Some(id) => {
                let owner = self.next_id;
                self.next_id += 1;
                for c in self.claims.iter_mut().flatten().filter(|c| c.id == id) {
                    c.name = name;
                    c.away_until = None;
                    if resumed.is_some() {
                        c.owner = owner;
                    }
                }
                id
            }
            None => {
                let slot = self.claims.iter_mut().find(|c| c.is_none()).ok_or(Error::Full)?;
                let id = self.next_id;
                self.next_id += 1;
                // xorshift over the seed, the id and the caller's entropy: not a secret
                // worth attacking, only something another client will not send by chance.
                let mut k = self.seed ^ id.wrapping_mul(0x9e37_79b9) ^ entropy.rotate_left(13);
                for _ in 0..3 {
                    k ^= k << 13;
                    k ^= k >> 17;
                    k ^= k << 5;
                }
                self.seed = k | 1;
                *slot = Some(Claim { id, name, key: k, owner: id, away_until: None });
                if self.holder.is_none() {
                    self.holder = Some(id);
                }
                id
            }
        };
        Ok((id, self.get(id).map_or(0, |c| c.owner)))
    }

    /// A stream is done with its claim (`mine`, as `claim` returned it): it closed, or
    /// sent `release`. A holder whose stream just closed (`dropped`) keeps control until
    /// `now_us + GRACE_US`, for it to come back; any other claim ends now. Nothing happens
    /// if another stream has taken the claim up since.
    pub fn leave(&mut self, mine: (u32, u32), dropped: bool, now_us: u64) {
        let holder = self.holder;
        let Some(c) = self.claims.iter_mut().flatten().find(|c| c.id == mine.0 && c.owner == mine.1) else { return };
        if dropped && holder == Some(c.id) {
            c.away_until = Some(now_us + GRACE_US);
        } else {
            self.release(mine.0);
        }
    }

    /// End the claims whose stream has been gone too long. True if anything changed.
    pub fn expire(&mut self, now_us: u64) -> bool {
        let mut changed = false;
        loop {
            let gone = self.claims().find(|c| c.away_until.is_some_and(|t| now_us >= t)).map(|c| c.id);
            let Some(id) = gone else { return changed };
            self.release(id);
            changed = true;
        }
    }

    /// End a claim. If it held control, the claim that has stood longest gets it.
    pub fn release(&mut self, id: u32) {
        let Some(at) = self.claims.iter().position(|c| c.is_some_and(|c| c.id == id)) else { return };
        // Close the gap, so the array stays oldest first.
        self.claims[at..].rotate_left(1);
        self.claims[N - 1] = None;
        if self.holder == Some(id) {
            self.holder = self.claims[0].map(|c| c.id);
        }
    }

    /// Hand control to a standing claim, or to nobody.
    pub fn grant(&mut self, id: Option<u32>) -> Result<(), Error> {
        if let Some(id) = id {
            self.get(id).ok_or(Error::NoSuchClaim)?;
        }
        self.holder = id;
        Ok(())
    }

    /// Whether a write that carries `key` (the `OFT-Control` header) may go ahead.
    pub fn allows(&self, key: Option<&str>) -> bool {
        match self.holder() {
            None => true,
            Some(h) => key.is_some_and(|k| k.trim().as_bytes().eq_ignore_ascii_case(&h.key_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<const N: usize>(c: &Control<N>, id: u32) -> std::string::String {
        std::string::String::from_utf8(c.get(id).unwrap().key_text().to_vec()).unwrap()
    }

    #[test]
    fn with_no_claims_anyone_may_write() {
        let c = Control::<4>::new(7);
        assert!(c.allows(None));
        assert!(c.allows(Some("whatever")));
        assert!(c.holder().is_none());
    }

    #[test]
    fn the_first_claim_holds_and_the_second_waits() {
        let mut c = Control::<4>::new(7);
        let rh = c.claim(None, "RotorHazard", None, 1).unwrap().0;
        let page = c.claim(None, "Timer page", None, 2).unwrap().0;
        assert_eq!(c.holder().unwrap().id, rh);
        assert!(!c.allows(None));
        assert!(c.allows(Some(&key(&c, rh))));
        assert!(!c.allows(Some(&key(&c, page))));
        assert_ne!(key(&c, rh), key(&c, page));
    }

    #[test]
    fn a_grant_moves_control_and_the_old_holder_gets_it_back_on_release() {
        let mut c = Control::<4>::new(7);
        let rh = c.claim(None, "RotorHazard", None, 1).unwrap().0;
        let page = c.claim(None, "Timer page", None, 2).unwrap().0;
        assert_eq!(c.holder().unwrap().id, rh, "a later claim never takes control by itself");
        c.grant(Some(page)).unwrap();
        assert_eq!(c.holder().unwrap().id, page);
        assert!(!c.allows(Some(&key(&c, rh))));
        c.release(page);
        assert_eq!(c.holder().unwrap().id, rh);
        c.release(rh);
        assert!(c.holder().is_none());
        assert_eq!(c.claims().count(), 0);
    }

    #[test]
    fn control_passes_to_the_claim_that_has_stood_longest() {
        let mut c = Control::<4>::new(7);
        let a = c.claim(None, "a", None, 1).unwrap().0;
        let b = c.claim(None, "b", None, 2).unwrap().0;
        let d = c.claim(None, "d", None, 3).unwrap().0;
        c.release(a);
        assert_eq!(c.holder().unwrap().id, b);
        assert_eq!(c.claims().map(|x| x.id).collect::<std::vec::Vec<_>>(), [b, d]);
    }

    #[test]
    fn grant_hands_control_to_a_claim_or_to_nobody() {
        let mut c = Control::<4>::new(7);
        let a = c.claim(None, "a", None, 1).unwrap().0;
        let b = c.claim(None, "b", None, 2).unwrap().0;
        c.grant(Some(b)).unwrap();
        assert_eq!(c.holder().unwrap().id, b);
        assert_eq!(c.grant(Some(99)), Err(Error::NoSuchClaim));
        assert_eq!(c.holder().unwrap().id, b);
        c.grant(None).unwrap();
        assert!(c.allows(None));
        assert_eq!(c.claims().count(), 2);
        // Freed control stays free for the standing claims; a new claim takes it.
        assert_eq!(c.claim(Some((a, a)), "a again", None, 3), Ok((a, a)));
        assert!(c.holder().is_none());
        let n = c.claim(None, "new", None, 4).unwrap().0;
        assert_eq!(c.holder().unwrap().id, n);
    }

    #[test]
    fn a_full_table_refuses_and_ids_are_not_reused() {
        let mut c = Control::<2>::new(7);
        let a = c.claim(None, "a", None, 1).unwrap().0;
        let b = c.claim(None, "b", None, 2).unwrap().0;
        assert_eq!(c.claim(None, "c", None, 3), Err(Error::Full));
        c.release(a);
        let d = c.claim(None, "d", None, 4).unwrap().0;
        assert!(d != a && d != b);
    }

    #[test]
    fn an_origin_matches_only_its_own_host() {
        assert!(same_origin("http://192.168.1.50:7270", "192.168.1.50:7270"));
        assert!(same_origin("HTTP://OFT-3FA2.local:7270", "oft-3fa2.local:7270"));
        assert!(!same_origin("http://192.168.1.50:5000", "192.168.1.50:7270"));
        assert!(!same_origin("http://evil.example", "192.168.1.50:7270"));
        assert!(!same_origin("null", "192.168.1.50:7270"));
        assert!(!same_origin("http://", ""));
    }

    #[test]
    fn names_are_made_safe_for_json() {
        assert_eq!(Name::new("  Rotor\"Hazard\\ \u{e9}\n ").as_str(), "RotorHazard ");
        assert_eq!(Name::new("").as_str(), "unnamed");
        assert_eq!(Name::new(&"x".repeat(50)).as_str().len(), NAME_MAX);
    }

    #[test]
    fn a_holder_that_drops_keeps_control_for_the_grace_period() {
        let mut c = Control::<4>::new(7);
        let rh = c.claim(None, "RotorHazard", None, 1).unwrap();
        let page = c.claim(None, "Timer page", None, 2).unwrap();
        let k = key(&c, rh.0);
        c.leave(rh, true, 1_000);
        assert!(!c.expire(1_000 + GRACE_US - 1));
        assert_eq!(c.holder().unwrap().id, rh.0);
        assert!(c.allows(Some(&k)), "its writes still go through while it is away");
        // It comes back on a new stream with its key: same claim, a new owner number.
        let back = c.claim(None, "RotorHazard", Some(&k), 3).unwrap();
        assert_eq!(back.0, rh.0);
        assert_ne!(back.1, rh.1);
        assert!(!c.expire(u64::MAX));
        assert_eq!(c.holder().unwrap().id, rh.0);
        // The old stream noticing its end late must not take the claim down.
        c.leave(rh, true, 2_000);
        c.leave(rh, false, 2_000);
        assert!(!c.expire(u64::MAX));
        assert_eq!(c.holder().unwrap().id, rh.0);
        // Gone for good: control passes on once the grace period is over.
        c.leave(back, true, 5_000);
        assert!(c.expire(5_000 + GRACE_US));
        assert_eq!(c.holder().unwrap().id, page.0);
    }

    #[test]
    fn a_waiting_claim_or_a_release_ends_at_once() {
        let mut c = Control::<4>::new(7);
        let rh = c.claim(None, "RotorHazard", None, 1).unwrap();
        let page = c.claim(None, "Timer page", None, 2).unwrap();
        c.leave(page, true, 0);
        assert_eq!(c.claims().count(), 1);
        c.leave(rh, false, 0);
        assert_eq!(c.claims().count(), 0);
        assert!(c.holder().is_none());
    }

    #[test]
    fn a_key_that_matches_nothing_makes_a_new_claim() {
        let mut c = Control::<4>::new(7);
        let a = c.claim(None, "a", Some("00000000"), 1).unwrap();
        let b = c.claim(None, "b", Some("nonsense"), 2).unwrap();
        assert_ne!(a.0, b.0);
        assert_eq!(c.holder().unwrap().id, a.0);
    }
}
