//! Anime-style face detection: a pure-Rust evaluator for nagadomi's `lbpcascade_animeface`
//! (an OpenCV LBP cascade, MIT), following OpenCV's `detectMultiScale`.

use std::str::FromStr;
use std::sync::OnceLock;

/// The cascade's detection window, in pixels.
const WINDOW: usize = 24;
/// Each scale's window is this much larger than the previous one.
const SCALE_STEP: f64 = 1.1;
/// A face needs more than this many overlapping hits; fewer are usually false positives.
const MIN_NEIGHBOURS: usize = 3;
/// How far (relative to size) two hits may differ and still count as the same face.
const GROUP_EPS: f64 = 0.2;

struct Stump {
    feature: usize,
    /// Bitset over the 256 LBP codes; a set bit selects `leaves[0]`.
    subset: [u32; 8],
    leaves: [f32; 2],
}

struct Stage {
    threshold: f32,
    stumps: Vec<Stump>,
}

struct Cascade {
    stages: Vec<Stage>,
    /// Top-left cell of each feature's 3x3 grid, plus the cell size: `[x, y, w, h]`.
    features: Vec<[usize; 4]>,
}

fn cascade() -> &'static Cascade {
    static CASCADE: OnceLock<Cascade> = OnceLock::new();
    CASCADE.get_or_init(|| parse(include_str!("lbpcascade_animeface.xml")))
}

/// Contents of every `<tag>` element, in document order.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let close = format!("</{tag}>");
    xml.split(&format!("<{tag}>")).skip(1).map(|s| s.split(&close).next().unwrap_or_default()).collect()
}

fn numbers<T: FromStr>(s: &str) -> Vec<T> {
    s.split_whitespace().map(|v| v.parse().ok().expect("cascade holds only numbers")).collect()
}

/// Reads an OpenCV cascade XML. Only depth-1 LBP trees (stumps) are supported, which is what
/// `opencv_traincascade -featureType LBP` produces by default.
fn parse(xml: &str) -> Cascade {
    let (_, rest) = xml.split_once("<stages>").expect("cascade has stages");
    let (stages, features) = rest.split_once("<features>").expect("cascade has features");
    // internalNodes: left, right, feature index, then the 8-word subset.
    let mut stumps =
        elements(stages, "internalNodes").into_iter().zip(elements(stages, "leafValues")).map(|(nodes, leaves)| {
            let nodes: Vec<i32> = numbers(nodes);
            let leaves: Vec<f32> = numbers(leaves);
            Stump {
                feature: nodes[2] as usize,
                subset: std::array::from_fn(|i| nodes[3 + i] as u32),
                leaves: [leaves[0], leaves[1]],
            }
        });
    let stages = elements(stages, "maxWeakCount")
        .into_iter()
        .zip(elements(stages, "stageThreshold"))
        .map(|(count, threshold)| Stage {
            // OpenCV shaves this epsilon off to absorb float rounding.
            threshold: numbers::<f32>(threshold)[0] - 1e-5,
            stumps: stumps.by_ref().take(numbers(count)[0]).collect(),
        })
        .collect();
    let features =
        elements(features, "rect").into_iter().map(|r| numbers(r).try_into().expect("rect has 4 numbers")).collect();
    Cascade { stages, features }
}

/// Summed-area table with a zero row and column in front.
struct Integral {
    stride: usize,
    sums: Vec<u32>,
}

impl Integral {
    fn new(pixels: &[u8], width: usize) -> Self {
        let stride = width + 1;
        let mut sums = vec![0; stride * (pixels.len() / width + 1)];
        for (y, row) in pixels.chunks_exact(width).enumerate() {
            let mut acc = 0;
            for (x, &p) in row.iter().enumerate() {
                acc += u32::from(p);
                sums[(y + 1) * stride + x + 1] = sums[y * stride + x + 1] + acc;
            }
        }
        Self { stride, sums }
    }

    fn sum(&self, x: usize, y: usize, w: usize, h: usize) -> u32 {
        let (top, bottom) = (y * self.stride, (y + h) * self.stride);
        self.sums[bottom + x + w] + self.sums[top + x] - self.sums[top + x + w] - self.sums[bottom + x]
    }
}

/// Local binary pattern of a 3x3 grid of `w`x`h` cells: one bit per outer cell, set when its sum
/// is at least the centre's, clockwise from the top-left cell as the high bit.
fn lbp(ii: &Integral, x: usize, y: usize, w: usize, h: usize) -> usize {
    let cell = |i: usize| ii.sum(x + i % 3 * w, y + i / 3 * h, w, h);
    let centre = cell(4);
    [0, 1, 2, 5, 8, 7, 6, 3].into_iter().fold(0, |code, i| code << 1 | usize::from(cell(i) >= centre))
}

/// Centre of the most confident face in an 8-bit greyscale image `width` pixels wide.
///
/// Expects histogram-equalised input, like the cascade was trained on.
pub fn detect(pixels: &[u8], width: usize) -> Option<(f64, f64)> {
    let cascade = cascade();
    let height = pixels.len() / width;
    let ii = Integral::new(pixels, width);
    let mut hits = Vec::new();
    // Scales the features instead of resampling the image: same windows, one integral image.
    let mut scale = 1.0;
    loop {
        let window = (WINDOW as f64 * scale) as usize;
        if window > width.min(height) {
            break;
        }
        // Truncation keeps every scaled 3x3 grid inside the window.
        let features: Vec<[usize; 4]> =
            cascade.features.iter().map(|f| f.map(|v| (v as f64 * scale) as usize)).collect();
        // OpenCV's stride: 2 pixels of the downscaled image, 1 once the scale passes 2.
        let step = ((if scale > 2.0 { scale } else { 2.0 * scale }) as usize).max(1);
        for y in (0..=height - window).step_by(step) {
            for x in (0..=width - window).step_by(step) {
                let face = cascade.stages.iter().all(|stage| {
                    let sum: f32 = stage
                        .stumps
                        .iter()
                        .map(|s| {
                            let [fx, fy, fw, fh] = features[s.feature];
                            let code = lbp(&ii, x + fx, y + fy, fw, fh);
                            s.leaves[usize::from(s.subset[code >> 5] & 1 << (code & 31) == 0)]
                        })
                        .sum();
                    sum >= stage.threshold
                });
                if face {
                    hits.push([x, y, window, window].map(|v| v as f64));
                }
            }
        }
        scale *= SCALE_STEP;
    }
    strongest(&hits).map(|[x, y, w, h]| (x + w / 2.0, y + h / 2.0))
}

/// Groups overlapping hits like OpenCV's `groupRectangles` and returns the mean box of the
/// largest group, if it has more than `MIN_NEIGHBOURS` hits.
// ponytail: O(n²) over raw hits; a few hundred at most at the sizes we detect on.
fn strongest(hits: &[[f64; 4]]) -> Option<[f64; 4]> {
    let similar = |a: &[f64; 4], b: &[f64; 4]| {
        let delta = GROUP_EPS * (a[2].min(b[2]) + a[3].min(b[3])) / 2.0;
        (a[0] - b[0]).abs() <= delta
            && (a[1] - b[1]).abs() <= delta
            && (a[0] + a[2] - b[0] - b[2]).abs() <= delta
            && (a[1] + a[3] - b[1] - b[3]).abs() <= delta
    };
    let mut parent: Vec<usize> = (0..hits.len()).collect();
    let root = |parent: &[usize], mut i: usize| {
        while parent[i] != i {
            i = parent[i];
        }
        i
    };
    for i in 0..hits.len() {
        for j in 0..i {
            if similar(&hits[i], &hits[j]) {
                let (ri, rj) = (root(&parent, i), root(&parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut groups = vec![(0, [0.0; 4]); hits.len()];
    for (i, hit) in hits.iter().enumerate() {
        let (count, sum) = &mut groups[root(&parent, i)];
        *count += 1;
        sum.iter_mut().zip(hit).for_each(|(s, v)| *s += v);
    }
    let (count, sum) = groups.into_iter().max_by_key(|(count, _)| *count)?;
    (count > MIN_NEIGHBOURS).then(|| sum.map(|v| v / count as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_parses_every_stage_and_feature() {
        let cascade = cascade();
        assert_eq!(cascade.stages.len(), 20);
        assert_eq!(cascade.stages.iter().map(|s| s.stumps.len()).sum::<usize>(), 771);
        assert!(cascade.stages.iter().flat_map(|s| &s.stumps).all(|s| s.feature < cascade.features.len()));
    }

    #[test]
    fn flat_image_has_no_face() {
        assert_eq!(detect(&[128; 64 * 48], 64), None);
    }

    #[test]
    fn lone_hit_is_not_a_face() {
        assert_eq!(strongest(&[[0.0, 0.0, 24.0, 24.0]]), None);
    }

    #[test]
    fn overlapping_hits_average_into_one_face() {
        let hits =
            [[10.0, 10.0, 24.0, 24.0], [11.0, 10.0, 24.0, 24.0], [10.0, 11.0, 24.0, 24.0], [9.0, 9.0, 24.0, 24.0]];
        assert_eq!(strongest(&hits), Some([10.0, 10.0, 24.0, 24.0]));
    }
}
