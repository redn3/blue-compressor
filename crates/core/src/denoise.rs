//! Noise reduction for Develop: a fast spatial filter on the four Bayer planes, before
//! white balance and color, so it works the same for N-RAW, R3D NE and ProRes RAW and for
//! the preview, playback and export alike.
//!
//! The frame is split into luma (the mean of the two greens), the green difference and two
//! color differences (R and B minus luma). Each is smoothed with a separable edge preserving
//! (bilateral) filter whose range threshold follows the sensor noise: photon noise grows with
//! the square root of the signal, and its scale is measured on every frame from the
//! difference of the two green samples of each 2x2 quad, which is almost pure noise.
//! Luma strength and chroma strength are separate, 0 (off) to 100.

use rayon::prelude::*;

use crate::develop::RawPlanes;

/// Signal floor (raw units above black) for the noise model, so the deep shadows do not
/// get a zero threshold.
const FLOOR: f32 = 32.0;

/// Noise variance per raw unit of signal, from the green difference: var(G1 - G2) = 2 a s.
pub fn noise_gain(pl: &RawPlanes) -> f32 {
    let n = pl.w * pl.h;
    if n == 0 {
        return 0.0;
    }
    let step = (n / 50_000).max(1);
    let mut r: Vec<f32> = (0..n)
        .step_by(step)
        .map(|i| {
            let (a, b) = (pl.data[0][i], pl.data[2][i]);
            let s = (0.5 * (a + b) - pl.black).max(FLOOR);
            let d = a - b;
            d * d / (2.0 * s)
        })
        .collect();
    let mid = r.len() / 2;
    let (_, m, _) = r.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    // The median of a chi-square (1 degree) sample is 0.455 of its mean.
    (*m / 0.455).max(1e-4)
}

/// One pass of a separable bilateral filter along rows (`horizontal`) or columns. `guide`
/// gives each pixel's noise sigma; `k` scales the range threshold.
fn pass(src: &[f32], w: usize, h: usize, radius: usize, sigma: &[f32], k: f32, horizontal: bool) -> Vec<f32> {
    let spatial: Vec<f32> = (0..=radius)
        .map(|i| {
            let x = i as f32 / (radius as f32 * 0.6 + 0.5);
            (-0.5 * x * x).exp()
        })
        .collect();
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let i = y * w + x;
            let c = src[i];
            let t = (k * sigma[i]).max(1e-3);
            let inv = 1.0 / (t * t);
            let (mut sum, mut wsum) = (c, 1.0f32);
            for d in 1..=radius {
                for dir in [-1isize, 1] {
                    let j = if horizontal {
                        let xx = (x as isize + dir * d as isize).clamp(0, w as isize - 1) as usize;
                        y * w + xx
                    } else {
                        let yy = (y as isize + dir * d as isize).clamp(0, h as isize - 1) as usize;
                        yy * w + x
                    };
                    let v = src[j];
                    let dv = v - c;
                    let q = 1.0 + dv * dv * inv;
                    let wt = spatial[d] / (q * q);
                    sum += wt * v;
                    wsum += wt;
                }
            }
            row[x] = sum / wsum;
        }
    });
    out
}

fn smooth(src: &[f32], w: usize, h: usize, radius: usize, sigma: &[f32], k: f32) -> Vec<f32> {
    let a = pass(src, w, h, radius, sigma, k, true);
    pass(&a, w, h, radius, sigma, k, false)
}

/// Reduces noise in place. `luma` and `chroma` are 0 (off) to 100.
pub fn denoise(pl: &mut RawPlanes, luma: f32, chroma: f32) {
    let (luma, chroma) = (luma.clamp(0.0, 100.0) / 100.0, chroma.clamp(0.0, 100.0) / 100.0);
    if (luma <= 0.0 && chroma <= 0.0) || pl.w < 4 || pl.h < 4 {
        return;
    }
    let (w, h, n) = (pl.w, pl.h, pl.w * pl.h);
    let a = noise_gain(pl);
    let black = pl.black;
    let [g1, r, g2, b] = &pl.data;
    let g: Vec<f32> = g1.par_iter().zip(g2.par_iter()).map(|(x, y)| 0.5 * (x + y)).collect();
    // Noise sigma of one raw sample at each quad.
    let sigma: Vec<f32> = g.par_iter().map(|v| (a * (v - black).max(FLOOR)).sqrt()).collect();
    let mut dg: Vec<f32> = g1.par_iter().zip(g2.par_iter()).map(|(x, y)| x - y).collect();
    let mut cr: Vec<f32> = r.par_iter().zip(g.par_iter()).map(|(x, y)| x - y).collect();
    let mut cb: Vec<f32> = b.par_iter().zip(g.par_iter()).map(|(x, y)| x - y).collect();
    let g = if luma > 0.0 {
        // Luma averages two samples: its noise is sigma / sqrt(2).
        let s: Vec<f32> = sigma.par_iter().map(|v| v * std::f32::consts::FRAC_1_SQRT_2).collect();
        let out = smooth(&g, w, h, 2, &s, 0.5 + 2.5 * luma);
        let keep = 1.0 - 0.8 * luma;
        dg.par_iter_mut().for_each(|v| *v *= keep);
        out
    } else {
        g
    };
    if chroma > 0.0 {
        // A color difference carries the noise of both samples.
        let s: Vec<f32> = sigma.par_iter().map(|v| v * 1.2247).collect();
        let radius = 2 + (chroma * 4.0).round() as usize;
        let k = 1.0 + 3.0 * chroma;
        cr = smooth(&cr, w, h, radius, &s, k);
        cb = smooth(&cb, w, h, radius, &s, k);
    }
    let mut data: [Vec<f32>; 4] = [vec![0.0; n], vec![0.0; n], vec![0.0; n], vec![0.0; n]];
    let [o1, or, o2, ob] = &mut data;
    o1.par_iter_mut().zip(o2.par_iter_mut()).enumerate().for_each(|(i, (x, y))| {
        *x = g[i] + 0.5 * dg[i];
        *y = g[i] - 0.5 * dg[i];
    });
    or.par_iter_mut().enumerate().for_each(|(i, v)| *v = g[i] + cr[i]);
    ob.par_iter_mut().enumerate().for_each(|(i, v)| *v = g[i] + cb[i]);
    pl.data = data;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(w: usize, h: usize) -> RawPlanes {
        // A flat gray with deterministic pseudo random noise of known size.
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 8) as f32 / (1u32 << 24) as f32) - 0.5
        };
        let data = [0, 1, 2, 3].map(|_| (0..w * h).map(|_| 1000.0 + 40.0 * rnd()).collect::<Vec<f32>>());
        RawPlanes { w, h, data, black: 0.0, white: 16383.0 }
    }

    fn stdev(v: &[f32]) -> f32 {
        let m = v.iter().sum::<f32>() / v.len() as f32;
        (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32).sqrt()
    }

    #[test]
    fn off_changes_nothing() {
        let mut p = noisy(32, 32);
        let before = p.data.clone();
        denoise(&mut p, 0.0, 0.0);
        assert_eq!(p.data, before);
    }

    #[test]
    fn reduces_noise_and_keeps_level() {
        let mut p = noisy(64, 64);
        let s0 = stdev(&p.data[1]);
        denoise(&mut p, 60.0, 60.0);
        let s1 = stdev(&p.data[1]);
        let mean = p.data[1].iter().sum::<f32>() / p.data[1].len() as f32;
        assert!(s1 < 0.6 * s0, "{s0} -> {s1}");
        assert!((mean - 1000.0).abs() < 2.0, "{mean}");
    }

    #[test]
    fn keeps_a_strong_edge() {
        let (w, h) = (32, 32);
        let data = [0, 1, 2, 3].map(|_| (0..w * h).map(|i| if i % w < w / 2 { 500.0 } else { 8000.0 }).collect());
        let mut p = RawPlanes { w, h, data, black: 0.0, white: 16383.0 };
        denoise(&mut p, 100.0, 100.0);
        assert!(p.data[1][w / 2 - 2] < 700.0 && p.data[1][w / 2 + 1] > 7500.0);
    }
}
