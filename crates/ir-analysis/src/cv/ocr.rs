//! Tiny fixed-font OCR for HUD text regions — built for `cl_showpos 1`
//! (position / view angles / velocity re-rendered every frame, top-left)
//! and reusable for the ammo counter later.
//!
//! Approach: binarize the ROI, segment text rows by horizontal projection,
//! segment glyphs by column gaps, and match each glyph against a small
//! template set by overlap score. Templates are *data*: captured once per
//! machine/HUD-scale into `showpos-glyphs.json` (the game's font is not
//! ours to ship); unit tests use an embedded synthetic font, so the whole
//! engine is verified without CS2 pixels.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::ShowposConfig;
use crate::cv::LumaFrame;

/// One glyph template: binarized row-major bitmap (0/1 per byte) of the
/// glyph's TIGHT bounding box (no cell padding) — matching gates on
/// absolute size, so '.' can never impersonate an '8'.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Glyph {
    pub w: u32,
    pub h: u32,
    /// Row-major, `w * h` entries of 0/1.
    pub data: Vec<u8>,
}

/// Character → template. Digits, '.', '-' cover showpos values; letters are
/// optional (unmatched glyphs are skipped, so labels like "vel" just fall
/// through unless captured).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlyphSet {
    pub glyphs: BTreeMap<char, Glyph>,
}

impl GlyphSet {
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_string(self)?)
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// A binarized region: 0/1 per byte, row-major.
struct Bitmap {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

/// White-text-on-anything binarization: threshold at mean + 0.9σ with a
/// floor, so bright HUD text separates from arbitrary scenery behind it.
fn binarize(frame: &LumaFrame) -> Bitmap {
    let n = frame.data.len().max(1) as f64;
    let mean = frame.data.iter().map(|&b| b as f64).sum::<f64>() / n;
    let var = frame
        .data
        .iter()
        .map(|&b| (b as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    let threshold = (mean + 0.9 * var.sqrt()).max(120.0);
    Bitmap {
        w: frame.w as usize,
        h: frame.h as usize,
        data: frame
            .data
            .iter()
            .map(|&b| u8::from(b as f64 > threshold))
            .collect(),
    }
}

/// Text rows: bands of consecutive rows whose on-pixel count clears a
/// noise floor, top to bottom.
fn text_rows(bm: &Bitmap) -> Vec<(usize, usize)> {
    let floor = (bm.w / 60).max(2);
    let on: Vec<bool> = (0..bm.h)
        .map(|r| {
            bm.data[r * bm.w..(r + 1) * bm.w]
                .iter()
                .filter(|&&v| v == 1)
                .count()
                >= floor
        })
        .collect();
    let mut bands = Vec::new();
    let mut start = None;
    for (r, &v) in on.iter().enumerate() {
        match (v, start) {
            (true, None) => start = Some(r),
            (false, Some(s)) => {
                if r - s >= 4 {
                    bands.push((s, r));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        if bm.h - s >= 4 {
            bands.push((s, bm.h));
        }
    }
    bands
}

/// Glyph column segments within a row band: runs of columns with any
/// on-pixel, separated by blank columns.
fn glyph_segments(bm: &Bitmap, band: (usize, usize)) -> Vec<(usize, usize)> {
    let on_col = |c: usize| (band.0..band.1).any(|r| bm.data[r * bm.w + c] == 1);
    let mut segs = Vec::new();
    let mut start = None;
    for c in 0..bm.w {
        match (on_col(c), start) {
            (true, None) => start = Some(c),
            (false, Some(s)) => {
                segs.push((s, c));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        segs.push((s, bm.w));
    }
    segs
}

/// Tight bounding box of a segment (trim blank rows), then nearest-neighbor
/// resample to the template's dimensions and score by pixel agreement.
fn match_glyph(bm: &Bitmap, band: (usize, usize), seg: (usize, usize), set: &GlyphSet) -> Option<(char, f64)> {
    let mut r0 = band.1;
    let mut r1 = band.0;
    for r in band.0..band.1 {
        if (seg.0..seg.1).any(|c| bm.data[r * bm.w + c] == 1) {
            r0 = r0.min(r);
            r1 = r1.max(r + 1);
        }
    }
    if r0 >= r1 {
        return None;
    }
    let (sw, sh) = (seg.1 - seg.0, r1 - r0);
    let mut best: Option<(char, f64)> = None;
    for (&ch, g) in &set.glyphs {
        let (gw, gh) = (g.w as usize, g.h as usize);
        // Fixed-font: a candidate must be roughly the template's absolute
        // size (small wobble from antialiasing only). This is what stops a
        // 3x3 '.' from resampling up into a convincing '8'.
        let close = |a: usize, b: usize| {
            let (a, b) = (a as f64, b as f64);
            a >= 0.6 * b && a <= 1.6 * b
        };
        if !close(sw, gw) || !close(sh, gh) {
            continue;
        }
        let mut agree = 0usize;
        for gy in 0..gh {
            let sy = r0 + gy * sh / gh;
            for gx in 0..gw {
                let sx = seg.0 + gx * sw / gw;
                if bm.data[sy * bm.w + sx] == g.data[gy * gw + gx] {
                    agree += 1;
                }
            }
        }
        let score = agree as f64 / (gw * gh) as f64;
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((ch, score));
        }
    }
    best
}

/// OCR one row band into text. Unrecognized glyphs (below `min_score`)
/// become spaces, and a gap wider than ~60% of the median glyph width
/// emits a space — without this, adjacent numbers concatenate.
fn read_row(bm: &Bitmap, band: (usize, usize), set: &GlyphSet, min_score: f64) -> String {
    let segs = glyph_segments(bm, band);
    let mut widths: Vec<usize> = segs.iter().map(|s| s.1 - s.0).collect();
    widths.sort_unstable();
    let median_w = widths.get(widths.len() / 2).copied().unwrap_or(1).max(1);
    let mut out = String::new();
    let mut prev_end: Option<usize> = None;
    for seg in segs {
        if let Some(end) = prev_end {
            if seg.0.saturating_sub(end) as f64 > 0.6 * median_w as f64 {
                out.push(' ');
            }
        }
        out.push(match match_glyph(bm, band, seg, set) {
            Some((ch, score)) if score >= min_score => ch,
            _ => ' ',
        });
        prev_end = Some(seg.1);
    }
    out
}

/// Numbers in OCR'd text, in order. Tolerates junk between them.
fn numbers(text: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' || (ch == '-' && cur.is_empty()) {
            cur.push(ch);
        } else if !cur.is_empty() {
            if let Ok(v) = cur.parse::<f64>() {
                out.push(v);
            }
            cur.clear();
        }
    }
    out
}

/// One frame's showpos readout. Fields are None when their line/values
/// didn't parse — partial reads are normal (occlusion, matching noise).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowposSample {
    pub t: f64,
    /// Velocity, u/s (the `vel:` line's last number).
    pub vel: Option<f64>,
    /// Pitch/yaw degrees (the `ang:` line's first two numbers).
    pub pitch_deg: Option<f64>,
    pub yaw_deg: Option<f64>,
}

/// Read the showpos overlay from every delivered ROI frame.
///
/// Line mapping follows `cl_showpos` order top-to-bottom: pos, ang, vel.
/// Values are sanity-gated (|vel| ≤ 3500 wraps CS2's fastest surf speeds;
/// pitch ∈ [-90, 90]) — a misread digit must not become a "measurement".
pub fn showpos_trace(rois: &[LumaFrame], set: &GlyphSet, cfg: &ShowposConfig) -> Vec<ShowposSample> {
    if set.is_empty() {
        return vec![];
    }
    rois.iter()
        .map(|frame| {
            let bm = binarize(frame);
            let bands = text_rows(&bm);
            let read = |i: usize| bands.get(i).map(|&b| read_row(&bm, b, set, cfg.min_score));
            let ang = read(1).map(|t| numbers(&t)).unwrap_or_default();
            let vel = read(2)
                .map(|t| numbers(&t))
                .unwrap_or_default()
                .last()
                .copied()
                .filter(|v| (0.0..=3500.0).contains(v));
            let pitch = ang.first().copied().filter(|p| (-90.0..=90.0).contains(p));
            let yaw = ang.get(1).copied().filter(|y| (-360.0..=360.0).contains(y));
            ShowposSample {
                t: frame.t_us as f64 / 1e6,
                vel,
                pitch_deg: pitch,
                yaw_deg: yaw,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 3×5 synthetic font (digits + '.', '-'), scaled up 3× when rendered —
    /// same shapes as the test pipeline's burned-in counter font.
    const FONT_3X5: &[(char, u16)] = &[
        ('0', 0b111_101_101_101_111),
        ('1', 0b010_110_010_010_111),
        ('2', 0b111_001_111_100_111),
        ('3', 0b111_001_111_001_111),
        ('4', 0b101_101_111_001_001),
        ('5', 0b111_100_111_001_111),
        ('6', 0b111_100_111_101_111),
        ('7', 0b111_001_010_010_010),
        ('8', 0b111_101_111_101_111),
        ('9', 0b111_101_111_001_111),
        ('.', 0b000_000_000_000_010),
        ('-', 0b000_000_111_000_000),
    ];
    const SCALE: usize = 3;

    pub(crate) fn test_glyphs() -> GlyphSet {
        let mut set = GlyphSet::default();
        for &(ch, bits) in FONT_3X5 {
            // Tight bounding box of the 3x5 pattern (matching gates on
            // absolute size, so templates must not carry cell padding).
            let cell = |r: usize, c: usize| bits >> (14 - (r * 3 + c)) & 1 == 1;
            let rows: Vec<usize> = (0..5).filter(|&r| (0..3).any(|c| cell(r, c))).collect();
            let cols: Vec<usize> = (0..3).filter(|&c| (0..5).any(|r| cell(r, c))).collect();
            let (r0, r1) = (rows[0], rows[rows.len() - 1] + 1);
            let (c0, c1) = (cols[0], cols[cols.len() - 1] + 1);
            let (gw, gh) = ((c1 - c0) * SCALE, (r1 - r0) * SCALE);
            let mut data = vec![0u8; gw * gh];
            for gr in r0..r1 {
                for gc in c0..c1 {
                    if cell(gr, gc) {
                        for dy in 0..SCALE {
                            for dx in 0..SCALE {
                                data[((gr - r0) * SCALE + dy) * gw + (gc - c0) * SCALE + dx] = 1;
                            }
                        }
                    }
                }
            }
            set.glyphs.insert(
                ch,
                Glyph {
                    w: gw as u32,
                    h: gh as u32,
                    data,
                },
            );
        }
        set
    }

    /// Render text lines into a luma ROI frame (white on dark, 2 px gaps).
    pub(crate) fn render_roi(t_us: u64, lines: &[&str]) -> LumaFrame {
        let (gw, gh) = (3 * SCALE, 5 * SCALE);
        let w = 220usize;
        let h = lines.len() * (gh + 6) + 4;
        let mut data = vec![24u8; w * h];
        let set = test_glyphs();
        for (li, line) in lines.iter().enumerate() {
            let y0 = 2 + li * (gh + 6);
            let mut x0 = 2usize;
            for ch in line.chars() {
                if ch == ' ' {
                    x0 += gw + 4;
                    continue;
                }
                if let Some(g) = set.glyphs.get(&ch) {
                    let (tw, th) = (g.w as usize, g.h as usize);
                    // Bottom-align like real text baselines.
                    let dy0 = y0 + gh - th;
                    for r in 0..th {
                        for c in 0..tw {
                            if g.data[r * tw + c] == 1 {
                                data[(dy0 + r) * w + x0 + c] = 250;
                            }
                        }
                    }
                    x0 += tw + 2;
                } else {
                    x0 += gw + 2;
                }
            }
        }
        LumaFrame {
            t_us,
            w: w as u32,
            h: h as u32,
            data,
        }
    }

    fn cfg() -> ShowposConfig {
        ShowposConfig::default()
    }

    #[test]
    fn reads_rendered_showpos_lines() {
        // pos / ang / vel lines, numbers only (labels unmatched = spaces).
        let roi = render_roi(500_000, &["100.5 -200.25 64.0", "-2.5 45.75 0.0", "249.9"]);
        let trace = showpos_trace(&[roi], &test_glyphs(), &cfg());
        assert_eq!(trace.len(), 1);
        let s = &trace[0];
        assert_eq!(s.vel, Some(249.9));
        assert_eq!(s.pitch_deg, Some(-2.5));
        assert_eq!(s.yaw_deg, Some(45.75));
    }

    #[test]
    fn implausible_values_are_rejected() {
        // vel 9999 (misread) and pitch 200 must not become measurements.
        let roi = render_roi(0, &["1 2 3", "200.0 45.0 0.0", "9999"]);
        let trace = showpos_trace(&[roi], &test_glyphs(), &cfg());
        assert_eq!(trace[0].vel, None);
        assert_eq!(trace[0].pitch_deg, None);
        assert_eq!(trace[0].yaw_deg, Some(45.0));
    }

    #[test]
    fn empty_glyphs_or_blank_roi_yield_nothing() {
        let roi = render_roi(0, &["1 2 3", "1 2 3", "100"]);
        assert!(showpos_trace(&[roi], &GlyphSet::default(), &cfg()).is_empty());
        let blank = LumaFrame {
            t_us: 0,
            w: 200,
            h: 60,
            data: vec![20; 200 * 60],
        };
        let trace = showpos_trace(&[blank], &test_glyphs(), &cfg());
        assert_eq!(trace[0].vel, None);
        assert_eq!(trace[0].yaw_deg, None);
    }

    #[test]
    fn glyph_set_roundtrips_through_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("glyphs.json");
        test_glyphs().save(&path).unwrap();
        let loaded = GlyphSet::load(&path).unwrap();
        assert_eq!(loaded.glyphs.len(), test_glyphs().glyphs.len());
    }

    #[test]
    fn number_extraction_tolerates_junk() {
        assert_eq!(numbers("vel  249.98"), vec![249.98]);
        assert_eq!(numbers("-2.5  45.75  0.0"), vec![-2.5, 45.75, 0.0]);
        assert_eq!(numbers("no digits"), Vec::<f64>::new());
    }
}
