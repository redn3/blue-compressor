//! User LUTs: `.cube` files (1D or 3D), applied to the encoded output signal.

use crate::{Error, Result};
use rayon::prelude::*;

/// A parsed `.cube` LUT.
#[derive(Debug, Clone)]
pub struct Lut {
    pub title: String,
    /// Size per axis (3D) or number of entries (1D).
    pub size: usize,
    pub three_d: bool,
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// RGB triplets, red fastest for 3D.
    pub table: Vec<[f32; 3]>,
}

impl Lut {
    /// Parses the text of a `.cube` file.
    pub fn parse(text: &str) -> Result<Lut> {
        let bad = |m: String| Error(format!("not a valid .cube LUT: {m}"));
        let (mut size1, mut size3) = (0usize, 0usize);
        let (mut min, mut max) = ([0.0f32; 3], [1.0f32; 3]);
        let mut title = String::new();
        let mut table = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let key = it.next().unwrap_or("");
            let rest: Vec<&str> = it.collect();
            let nums = |r: &[&str]| -> Result<[f32; 3]> {
                if r.len() < 3 {
                    return Err(bad(format!("line {} needs three numbers", n + 1)));
                }
                let mut v = [0.0; 3];
                for k in 0..3 {
                    v[k] = r[k].parse::<f32>().map_err(|_| bad(format!("line {}: {}", n + 1, r[k])))?;
                    if !v[k].is_finite() {
                        return Err(bad(format!("line {}", n + 1)));
                    }
                }
                Ok(v)
            };
            match key.to_ascii_uppercase().as_str() {
                "TITLE" => title = line[5..].trim().trim_matches('"').to_string(),
                "LUT_1D_SIZE" => {
                    size1 = rest.first().and_then(|v| v.parse().ok()).ok_or_else(|| bad("LUT_1D_SIZE".into()))?
                }
                "LUT_3D_SIZE" => {
                    size3 = rest.first().and_then(|v| v.parse().ok()).ok_or_else(|| bad("LUT_3D_SIZE".into()))?
                }
                "DOMAIN_MIN" => min = nums(&rest)?,
                "DOMAIN_MAX" => max = nums(&rest)?,
                "LUT_1D_INPUT_RANGE" | "LUT_3D_INPUT_RANGE" => {
                    let a: Vec<f32> = rest.iter().filter_map(|v| v.parse().ok()).collect();
                    if a.len() != 2 {
                        return Err(bad(key.into()));
                    }
                    min = [a[0]; 3];
                    max = [a[1]; 3];
                }
                k if k.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                    let mut all = vec![key];
                    all.extend(rest);
                    table.push(nums(&all)?);
                }
                _ => {} // Unknown keywords are allowed by the format.
            }
        }
        let (three_d, size) = match (size1, size3) {
            (0, 0) => return Err(bad("no LUT_1D_SIZE or LUT_3D_SIZE".into())),
            (_, s) if s > 0 => (true, s),
            (s, _) => (false, s),
        };
        if (three_d && !(2..=256).contains(&size)) || (!three_d && !(2..=65536).contains(&size)) {
            return Err(bad(format!("size {size} is out of range")));
        }
        let want = if three_d { size * size * size } else { size };
        if table.len() != want {
            return Err(bad(format!("{} entries, expected {want}", table.len())));
        }
        if (0..3).any(|k| max[k] <= min[k]) {
            return Err(bad("DOMAIN_MAX must be above DOMAIN_MIN".into()));
        }
        Ok(Lut { title, size, three_d, min, max, table })
    }

    /// Reads and parses a `.cube` file.
    pub fn load(path: &std::path::Path) -> Result<Lut> {
        let text = std::fs::read_to_string(path).map_err(|e| Error(format!("cannot read {}: {e}", path.display())))?;
        Lut::parse(&text)
    }

    /// One RGB value (0..1) through the LUT.
    pub fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let pos = |k: usize| ((v[k] - self.min[k]) / (self.max[k] - self.min[k])).clamp(0.0, 1.0) * (n - 1) as f32;
        if !self.three_d {
            return [0, 1, 2].map(|k| {
                let p = pos(k);
                let i = (p as usize).min(n - 2);
                let f = p - i as f32;
                self.table[i][k] + (self.table[i + 1][k] - self.table[i][k]) * f
            });
        }
        let (r, g, b) = (pos(0), pos(1), pos(2));
        let (r0, g0, b0) = ((r as usize).min(n - 2), (g as usize).min(n - 2), (b as usize).min(n - 2));
        let (fr, fg, fb) = (r - r0 as f32, g - g0 as f32, b - b0 as f32);
        let at = |ri: usize, gi: usize, bi: usize| self.table[(bi * n + gi) * n + ri];
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|k| a[k] + (b[k] - a[k]) * t);
        let c00 = lerp(at(r0, g0, b0), at(r0 + 1, g0, b0), fr);
        let c10 = lerp(at(r0, g0 + 1, b0), at(r0 + 1, g0 + 1, b0), fr);
        let c01 = lerp(at(r0, g0, b0 + 1), at(r0 + 1, g0, b0 + 1), fr);
        let c11 = lerp(at(r0, g0 + 1, b0 + 1), at(r0 + 1, g0 + 1, b0 + 1), fr);
        lerp(lerp(c00, c10, fg), lerp(c01, c11, fg), fb)
    }

    /// Applies the LUT in place to 16-bit RGB.
    pub fn apply_rgb16(&self, img: &mut [u16]) {
        img.par_chunks_mut(3 * 1024).for_each(|ch| {
            for px in ch.chunks_mut(3) {
                let o = self.apply([px[0], px[1], px[2]].map(|c| c as f32 / 65535.0));
                for k in 0..3 {
                    px[k] = (o[k].clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity3(n: usize) -> String {
        let mut s = format!("TITLE \"id\"\nLUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let f = |i: usize| i as f32 / (n - 1) as f32;
                    s += &format!("{} {} {}\n", f(r), f(g), f(b));
                }
            }
        }
        s
    }

    #[test]
    fn identity_3d_is_identity() {
        let l = Lut::parse(&identity3(17)).unwrap();
        assert_eq!(l.title, "id");
        let o = l.apply([0.2, 0.5, 0.9]);
        for (a, b) in o.iter().zip([0.2, 0.5, 0.9]) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn one_d_inverts() {
        let l = Lut::parse("LUT_1D_SIZE 2\n1 1 1\n0 0 0\n").unwrap();
        let o = l.apply([0.25, 0.5, 1.0]);
        assert!((o[0] - 0.75).abs() < 1e-6 && (o[2]).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_files() {
        assert!(Lut::parse("hello").is_err());
        assert!(Lut::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
        assert!(Lut::parse("LUT_1D_SIZE 2\n0 0 x\n1 1 1\n").is_err());
    }
}
