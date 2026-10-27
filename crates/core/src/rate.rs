//! Rate control for a target size in bits per Bayer sample (bpp), the way the Nikon HE and
//! HE* still formats are described: bpp = 8 * codestream bytes / (Wf * Hf), and each
//! precinct is coded to fit a byte budget by choosing its quantization.
//!
//! A first pass codes the frame at the finest allowed rank (for a requantized clip that is
//! the lossless repack, which keeps the camera's own rank). When that already fits, it is
//! the output. Otherwise the budget, less the codestream header, slice headers and end
//! marker, is split over the slices and then over the precincts in proportion to their
//! first pass sizes, so detailed precincts get more bytes than flat ones, and whatever a
//! precinct does not use carries forward to the rest of its slice. Each precinct takes the
//! finest truncation rank that fits its share, found by galloping from a guess and then
//! bisecting. The guess is the rank the same precinct took in the previous frame of the
//! clip (consecutive video frames code alike), or for the first frame of a run the
//! previous precinct's rank. A guess from the previous frame that fits with little to
//! spare is taken as is, so most precincts of a video are encoded once or twice.
//! Slices are independent and every decision depends only on the frame data and on which
//! frame came before (frames are rate controlled in fixed runs, see `transcode`), so the
//! output is the same for any thread count.
//!
//! Background on bpp as a measure: "Nikon HE and HE* RAW, Format analysis and decoding
//! reference", sections 2.1 and 2.2, Copyright (c) 2026 patrol_taking_9j, CC BY-SA 4.0.

use crate::tico::{Layout, PredState, TRUNCATION_MAX};
use crate::Result;

/// One precinct coded at `rank`: its bytes and the prediction state after it.
pub(crate) struct Coded {
    pub rank: i32,
    pub bytes: Vec<u8>,
    pub state: PredState,
}

/// Coarsest rank worth trying: every band truncated at the maximum.
pub(crate) fn max_rank(l: &Layout) -> i32 {
    let nb = l.bands.len() as i32;
    let max_gain = l.bands.iter().map(|b| b.gain as i32).max().unwrap_or(0);
    let q = (TRUNCATION_MAX + max_gain + 1).min(255);
    q * nb + nb
}

/// Bytes available to the precincts of each slice for a codestream of `budget` bytes, in
/// proportion to the precincts' `weights` (their size at the finest allowed rank).
pub(crate) fn slice_budgets(l: &Layout, header_bytes: usize, budget: usize, weights: &[usize]) -> Vec<usize> {
    let overhead = header_bytes + 6 * l.slices + 2;
    let avail = budget.saturating_sub(overhead) as u128;
    let total: u128 = weights.iter().map(|&w| w as u128).sum::<u128>().max(1);
    (0..l.slices)
        .map(|s| {
            let p0 = s * l.slice_height;
            let p1 = l.precincts.min(p0 + l.slice_height);
            let w: u128 = weights[p0..p1].iter().map(|&w| w as u128).sum();
            (avail * w / total) as usize
        })
        .collect()
}

/// The finest rank in [lo, hi] whose coding fits `budget` bytes, assuming coded size does
/// not grow with rank; `hi` when nothing fits. `code(rank)` encodes one candidate. When the
/// `hint` itself fits with at most `slack` bytes to spare it is taken without trying finer
/// ranks (a good guess from the previous frame then costs one encode instead of two).
pub(crate) fn finest_fitting(
    lo: i32,
    hi: i32,
    hint: i32,
    budget: usize,
    slack: usize,
    mut code: impl FnMut(i32) -> Result<Coded>,
) -> Result<Coded> {
    let hi = hi.max(lo);
    let fits = |c: &Coded| c.bytes.len() <= budget;
    let first = code(hint.clamp(lo, hi))?;
    if fits(&first) && budget - first.bytes.len() <= slack {
        return Ok(first);
    }
    let (mut bad, mut good) = if fits(&first) {
        // Gallop towards finer ranks until one does not fit.
        let mut good = first;
        let mut step = 1;
        loop {
            if good.rank == lo {
                return Ok(good);
            }
            let probe = (good.rank - step).max(lo);
            let c = code(probe)?;
            if fits(&c) {
                good = c;
                step *= 2;
            } else {
                break (probe, good);
            }
        }
    } else {
        // Gallop towards coarser ranks until one fits.
        let mut bad = first.rank;
        let mut last = first;
        let mut step = 1;
        loop {
            if bad == hi {
                return Ok(last);
            }
            let probe = (bad + step).min(hi);
            let c = code(probe)?;
            if fits(&c) {
                break (bad, c);
            }
            bad = probe;
            last = c;
            step *= 2;
        }
    };
    while bad + 1 < good.rank {
        let mid = bad + (good.rank - bad) / 2;
        let c = code(mid)?;
        if fits(&c) {
            good = c;
        } else {
            bad = mid;
        }
    }
    Ok(good)
}

/// Bytes a precinct may leave unused when its guess comes from the previous frame: what it
/// leaves goes to the precincts after it, so a small slack costs next to nothing in size.
pub(crate) fn seed_slack(share: usize) -> usize {
    share / 32
}

/// Per-slice budget bookkeeping: each precinct gets the bytes left times its share of the
/// weight left, so what one precinct does not use carries forward to the rest of the slice.
pub(crate) struct SliceBudget {
    left: usize,
    weight: u128,
}

impl SliceBudget {
    pub fn new(bytes: usize, weights: &[usize]) -> SliceBudget {
        SliceBudget { left: bytes, weight: weights.iter().map(|&w| w as u128).sum() }
    }

    pub fn share(&self, weight: usize) -> usize {
        if self.weight == 0 {
            return self.left;
        }
        (self.left as u128 * weight as u128 / self.weight) as usize
    }

    pub fn spend(&mut self, bytes: usize, weight: usize) {
        self.left = self.left.saturating_sub(bytes);
        self.weight = self.weight.saturating_sub(weight as u128);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(rank: i32) -> Result<Coded> {
        // Size falls with rank: 10000 / (rank + 1).
        Ok(Coded { rank, bytes: vec![0; 10000 / (rank as usize + 1)], state: PredState::default() })
    }

    #[test]
    fn finds_the_finest_fitting_rank_from_any_hint() {
        for budget in [50usize, 100, 333, 1000, 20000] {
            let want = (0..=200).find(|&r| 10000 / (r as usize + 1) <= budget).unwrap_or(200);
            for hint in [0, 3, 40, 99, 150, 200] {
                assert_eq!(
                    finest_fitting(0, 200, hint, budget, 0, fake).unwrap().rank,
                    want,
                    "budget {budget} hint {hint}"
                );
            }
        }
    }

    #[test]
    fn nothing_fits_gives_the_coarsest() {
        assert_eq!(finest_fitting(10, 20, 12, 1, 0, fake).unwrap().rank, 20);
    }

    #[test]
    fn a_close_hint_is_taken_as_is() {
        // Rank 9 codes 1000 bytes: within 50 of a 1040 byte budget, so no finer try.
        let mut tries = 0;
        let c = finest_fitting(0, 200, 9, 1040, 50, |r| {
            tries += 1;
            fake(r)
        })
        .unwrap();
        assert_eq!((c.rank, tries), (9, 1));
        // Without slack the search goes on and finds the same rank (8 codes 1111 bytes).
        assert_eq!(finest_fitting(0, 200, 9, 1040, 0, fake).unwrap().rank, 9);
    }
}
