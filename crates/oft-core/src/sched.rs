//! Seat scheduling (spec §4.1): which Node watches which seat.
//!
//! - Seats ≤ Nodes: one seat per Node, no hopping.
//! - Seats > Nodes: the seats are spread over the Nodes so that no Node needs more **tuning
//!   positions** than it has to. Channels up to [`POSITION_SPAN_MHZ`] apart share one
//!   position, so neighbouring channels go to the same Node.
//! - Second priority: a seat stays on the Node it was on.
//!
//! Pure and deterministic: the same seats, Nodes and previous assignment always give the
//! same answer.

use crate::host::MAX_SEATS;

/// Channels this far apart, or closer, are sampled from one tuning position.
pub const POSITION_SPAN_MHZ: u16 = 40;

/// How many tuning positions a Node needs for these frequencies (sorted ascending).
pub fn positions(sorted_mhz: &[u16]) -> usize {
    let mut n = 0;
    let mut start = 0u16;
    for &f in sorted_mhz {
        if n == 0 || f - start > POSITION_SPAN_MHZ {
            n += 1;
            start = f;
        }
    }
    n
}

/// Assign `seats` (`(seat, freq_mhz)`, any order, seat numbers below [`MAX_SEATS`]) to Nodes.
///
/// `nodes[k]` is `Some(max_seats)` for a Node that can take seats and `None` for one that
/// cannot (absent, silent, switched off). `prev[seat]` is the Node the seat is on now.
/// Returns the Node for each seat number; `None` for seats not asked for, or when there is
/// no Node to take one.
pub fn assign<const N: usize>(seats: &[(u8, u16)], nodes: &[Option<u8>; N], prev: &[Option<u8>; MAX_SEATS]) -> [Option<u8>; MAX_SEATS] {
    let mut result = [None; MAX_SEATS];

    // The seats by frequency (then seat number, so the order is fixed).
    let mut sorted = [(0u8, 0u16); MAX_SEATS];
    let mut n = 0;
    for &(seat, freq) in seats {
        if seat as usize >= MAX_SEATS || n == MAX_SEATS || sorted[..n].iter().any(|s| s.0 == seat) {
            continue;
        }
        let mut k = n;
        while k > 0 && (sorted[k - 1].1, sorted[k - 1].0) > (freq, seat) {
            sorted[k] = sorted[k - 1];
            k -= 1;
        }
        sorted[k] = (seat, freq);
        n += 1;
    }
    let sorted = &sorted[..n];

    let usable = nodes.iter().flatten().filter(|&&cap| cap > 0).count();
    if n == 0 || usable == 0 {
        return result;
    }
    let groups = usable.min(n);

    // Every way to cut the sorted seats into `groups` runs: a bit set in `cuts` ends a run
    // after that seat. At most 2^7 candidates.
    let mut best: Option<(Cost, [Option<u8>; MAX_SEATS])> = None;
    for cuts in 0u32..(1 << (n - 1)) {
        if cuts.count_ones() as usize != groups - 1 {
            continue;
        }
        let mut cost = Cost::default();
        let mut candidate = [None; MAX_SEATS];
        let mut used = [false; N];
        let mut start = 0;
        for end in 1..=n {
            if end < n && cuts & (1 << (end - 1)) == 0 {
                continue;
            }
            let run = &sorted[start..end];
            start = end;

            let mut freqs = [0u16; MAX_SEATS];
            for (f, s) in freqs.iter_mut().zip(run) {
                *f = s.1;
            }
            let p = positions(&freqs[..run.len()]);
            cost.max_positions = cost.max_positions.max(p);
            cost.positions += p;
            cost.max_seats = cost.max_seats.max(run.len());

            // The free Node most of this run's seats are already on; the lowest on a tie.
            let mut pick: Option<(usize, usize)> = None;
            for (k, cap) in nodes.iter().enumerate() {
                if used[k] || !cap.is_some_and(|c| c > 0) {
                    continue;
                }
                let stay = run.iter().filter(|s| prev[s.0 as usize] == Some(k as u8)).count();
                if pick.map_or(true, |(_, best_stay)| stay > best_stay) {
                    pick = Some((k, stay));
                }
            }
            let Some((node, stay)) = pick else { continue };
            used[node] = true;
            let cap = nodes[node].unwrap_or(0) as usize;
            // A Node takes no more seats than it says it can; the rest of the run goes unserved.
            cost.unserved += run.len().saturating_sub(cap);
            cost.moved += run.len().min(cap).saturating_sub(stay);
            for s in run.iter().take(cap) {
                candidate[s.0 as usize] = Some(node as u8);
            }
        }
        if best.as_ref().map_or(true, |(b, _)| cost < *b) {
            best = Some((cost, candidate));
        }
    }
    if let Some((_, candidate)) = best {
        result = candidate;
    }
    result
}

/// What makes one assignment better than another, most important first.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Cost {
    /// Seats no Node had room for.
    unserved: usize,
    /// Tuning positions on the busiest Node: what sets the worst seat's sample rate.
    max_positions: usize,
    /// Seats on the fullest Node.
    max_seats: usize,
    /// Tuning positions over all Nodes.
    positions: usize,
    /// Seats that change Node.
    moved: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [Option<u8>; MAX_SEATS] = [None; MAX_SEATS];
    // Raceband 1..8.
    const R: [u16; 8] = [5658, 5695, 5732, 5769, 5806, 5843, 5880, 5917];

    fn seats(freqs: &[u16]) -> std::vec::Vec<(u8, u16)> {
        freqs.iter().enumerate().map(|(k, &f)| (k as u8, f)).collect()
    }

    #[test]
    fn positions_counts_spans_of_forty() {
        assert_eq!(positions(&[]), 0);
        assert_eq!(positions(&[5800]), 1);
        assert_eq!(positions(&[5658, 5695]), 1);
        assert_eq!(positions(&[5658, 5695, 5732]), 2);
        assert_eq!(positions(&R), 4);
    }

    #[test]
    fn one_seat_per_node_when_there_are_enough_nodes() {
        let a = assign(&seats(&R[..2]), &[Some(8), Some(8)], &NONE);
        assert_eq!(&a[..3], &[Some(0), Some(1), None]);
        // Fewer seats than Nodes: the spare Node gets nothing.
        let a = assign(&seats(&R[..1]), &[Some(8), Some(8)], &NONE);
        assert_eq!(&a[..2], &[Some(0), None]);
    }

    #[test]
    fn one_node_takes_every_seat() {
        let a = assign(&seats(&R), &[Some(8)], &NONE);
        assert!(a.iter().all(|&n| n == Some(0)));
        // The same with the other slots empty.
        let a = assign(&seats(&R[..4]), &[None, Some(8), None, None], &NONE);
        assert_eq!(&a[..5], &[Some(1), Some(1), Some(1), Some(1), None]);
    }

    #[test]
    fn neighbouring_channels_share_a_node() {
        // R1 R2 | R7 R8 on two Nodes: each Node needs one position.
        let a = assign(&[(0, R[0]), (1, R[6]), (2, R[1]), (3, R[7])], &[Some(8), Some(8)], &NONE);
        assert_eq!(a[0], a[2]);
        assert_eq!(a[1], a[3]);
        assert_ne!(a[0], a[1]);
        // Eight seats on four Nodes: pairs, one position each.
        let a = assign(&seats(&R), &[Some(8); 4], &NONE);
        assert_eq!(a, [Some(0), Some(0), Some(1), Some(1), Some(2), Some(2), Some(3), Some(3)]);
        // Eight seats on two Nodes: four each, two positions each.
        let a = assign(&seats(&R), &[Some(8); 2], &NONE);
        assert_eq!(a, [Some(0), Some(0), Some(0), Some(0), Some(1), Some(1), Some(1), Some(1)]);
    }

    #[test]
    fn seats_stay_on_their_node() {
        // Two seats on Nodes 1 and 0 (swapped from the default order) stay there.
        let mut prev = NONE;
        prev[0] = Some(1);
        prev[1] = Some(0);
        let a = assign(&seats(&R[..2]), &[Some(8), Some(8)], &prev);
        assert_eq!(&a[..2], &[Some(1), Some(0)]);
        // Three far-apart seats on two Nodes can split 1+2 or 2+1; the split in use is kept.
        let far = [(0u8, 5658u16), (1, 5769), (2, 5880)];
        let first = assign(&far, &[Some(8), Some(8)], &NONE);
        let mut other = NONE;
        other[0] = Some(0);
        other[1] = Some(0);
        other[2] = Some(1);
        assert_eq!(assign(&far, &[Some(8), Some(8)], &other), other);
        let mut flipped = NONE;
        flipped[0] = Some(0);
        flipped[1] = Some(1);
        flipped[2] = Some(1);
        assert_eq!(assign(&far, &[Some(8), Some(8)], &flipped), flipped);
        assert!(first == other || first == flipped);
    }

    #[test]
    fn a_lost_node_hands_its_seats_over_and_takes_them_back() {
        let two = assign(&seats(&R[..2]), &[Some(8), Some(8)], &NONE);
        let one = assign(&seats(&R[..2]), &[Some(8), None], &two);
        assert_eq!(&one[..2], &[Some(0), Some(0)]);
        let back = assign(&seats(&R[..2]), &[Some(8), Some(8)], &one);
        // Seat 0 was on Node 0 and stays; Node 1 gets the other.
        assert_eq!(&back[..2], &[Some(0), Some(1)]);
    }

    #[test]
    fn a_node_never_gets_more_seats_than_it_can_take() {
        // One receiver each (max_seats 1), three seats: one goes unserved.
        let a = assign(&seats(&R[..3]), &[Some(1), Some(1)], &NONE);
        assert_eq!(a.iter().flatten().count(), 2);
        // A small Node beside a big one: nothing is left out.
        let a = assign(&seats(&R[..5]), &[Some(1), Some(8)], &NONE);
        assert_eq!(a.iter().flatten().count(), 5);
        assert_eq!(a.iter().filter(|&&n| n == Some(0)).count(), 1);
    }

    #[test]
    fn nothing_to_do() {
        assert_eq!(assign::<2>(&[], &[Some(8), Some(8)], &NONE), NONE);
        assert_eq!(assign::<2>(&seats(&R), &[None, None], &NONE), NONE);
        // Seat numbers out of range and repeats are ignored.
        let a = assign(&[(9, 5800), (0, 5800), (0, 5700)], &[Some(8)], &NONE);
        assert_eq!(a.iter().flatten().count(), 1);
    }
}
