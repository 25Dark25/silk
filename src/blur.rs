//! Motion blur in the style of ReelSmart Motion Blur: every pixel is smeared
//! along its own motion path while a virtual shutter is open.
//!
//! The motion comes from RIFE's optical flow (see `rife.rs`), re-anchored to
//! each output frame. `motion_blur` reconstructs the exposure per pixel from
//! its own trail and the trails of objects in front sweeping over it, in the
//! spirit of McGuire et al., "A Reconstruction Filter for Plausible Motion
//! Blur" (2012), with a depth cue suited to gameplay. Averaging happens in
//! linear light, so bright highlights streak like a real camera's instead of
//! turning grey.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Displacement in pixels over one source-frame interval, planar `[x | y]`.
pub type Motion = Vec<f32>;

/// Longest trail sampled at full density (one sample per pixel of trail);
/// longer trails reuse this many samples, smoothed by bilinear filtering.
const MAX_SAMPLES: usize = 64;

const TILE: usize = 16;

/// A pair of frames' motion, seen from the pair's midpoint, where RIFE's
/// flow is reliable (near t = 0 or 1 it ignores the far frame).
pub struct PairMotion {
    width: usize,
    height: usize,
    /// A→B displacement per midpoint pixel, planar `[x | y]`.
    motion: Motion,
    /// RIFE's midpoint frame, planar RGB, used to re-anchor the motion.
    image: Vec<f32>,
    /// Fastest motion around each 16×16 tile (3×3 tiles), planar `[x | y]`.
    tile_max: Motion,
    tiles_x: usize,
    tiles_y: usize,
}

impl PairMotion {
    /// `flow` is RIFE's `[to A x | to A y | to B x | to B y]` at t = 0.5.
    pub fn new(flow: &[f32], image: Vec<f32>, width: usize, height: usize) -> Self {
        let plane = width * height;
        let (to_a, to_b) = flow.split_at(2 * plane);
        let mut motion: Motion = vec![0f32; 2 * plane];
        parallel_rows(&mut motion, 2, width, height, |y, row| {
            for (c, out) in row.iter_mut().enumerate() {
                let start = c * plane + y * width;
                for (x, o) in out.iter_mut().enumerate() {
                    *o = to_b[start + x] - to_a[start + x];
                }
            }
        });

        let (tiles_x, tiles_y) = (width.div_ceil(TILE), height.div_ceil(TILE));
        let (mx, my) = motion.split_at(plane);
        let fastest: Vec<(f32, f32)> = parallel_map(tiles_y, |ty| {
            let mut band = vec![(0f32, 0f32); tiles_x];
            for y in ty * TILE..((ty + 1) * TILE).min(height) {
                for x in 0..width {
                    let v = (mx[y * width + x], my[y * width + x]);
                    let t = &mut band[x / TILE];
                    if v.0.hypot(v.1) > t.0.hypot(t.1) {
                        *t = v;
                    }
                }
            }
            band
        })
        .concat();
        let tiles = tiles_x * tiles_y;
        let mut tile_max = vec![0f32; 2 * tiles];
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let mut best = (0f32, 0f32);
                for ny in ty.saturating_sub(1)..(ty + 2).min(tiles_y) {
                    for nx in tx.saturating_sub(1)..(tx + 2).min(tiles_x) {
                        let v = fastest[ny * tiles_x + nx];
                        if v.0.hypot(v.1) > best.0.hypot(best.1) {
                            best = v;
                        }
                    }
                }
                tile_max[ty * tiles_x + tx] = best.0;
                tile_max[tiles + ty * tiles_x + tx] = best.1;
            }
        }
        Self { width, height, motion, image, tile_max, tiles_x, tiles_y }
    }

    /// The motion of each pixel of the frame at time `tau` of the pair (0 = A,
    /// 1 = B), per source-frame interval when `scale` is 1 / the pair's span.
    ///
    /// The point at `x` at time `tau` sits at `x + (0.5 − tau)·V` at the
    /// midpoint, with `V` its own (unknown) motion. Several candidates for `V`
    /// are tried and the one whose midpoint position shows the same colour
    /// as `frame` at `x` wins. That keeps a static HUD static and gives the
    /// trailing edge of a moving object its motion, both of which a plain
    /// lookup gets wrong.
    pub fn at(&self, frame: &[f32], tau: f32, scale: f32) -> Motion {
        let (width, height) = (self.width, self.height);
        let shift = 0.5 - tau;
        let mut out = vec![0f32; 2 * width * height];
        if shift == 0.0 {
            out.iter_mut().zip(&self.motion).for_each(|(o, v)| *o = v * scale);
            return out;
        }
        parallel_rows(&mut out, 2, width, height, |y, row| {
            let [out_x, out_y] = row else { unreachable!("two planes") };
            for (x, (ox, oy)) in out_x.iter_mut().zip(out_y.iter_mut()).enumerate() {
                let (vx, vy) = self.motion_of(frame, x, y, shift);
                *ox = vx * scale;
                *oy = vy * scale;
            }
        });
        out
    }

    fn motion_of(&self, frame: &[f32], x: usize, y: usize, shift: f32) -> (f32, f32) {
        let (width, height) = (self.width, self.height);
        let plane = width * height;
        let (mx, my) = self.motion.split_at(plane);
        let i = y * width + x;
        let (fx, fy) = (x as f32, y as f32);
        let tile = (y / TILE).min(self.tiles_y - 1) * self.tiles_x + (x / TILE).min(self.tiles_x - 1);
        let own = (mx[i], my[i]);
        let tap = Tap::new(width, height, fx + shift * own.0, fy + shift * own.1);
        let looked_up = (tap.sample(mx), tap.sample(my));
        let fastest = (self.tile_max[tile], self.tile_max[self.tiles_x * self.tiles_y + tile]);
        // Inside a smoothly moving region (a camera turn) all candidates
        // agree and the plain lookup is the answer.
        let agree = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).hypot(a.1 - b.1) < 0.5;
        if agree(own, looked_up) && agree(own, fastest) {
            return looked_up;
        }

        let mut best = (f32::INFINITY, (0.0, 0.0));
        for (n, c) in [own, looked_up, fastest].into_iter().enumerate() {
            let tap = Tap::new(width, height, fx + shift * c.0, fy + shift * c.1);
            // The candidate must explain the colour and be the motion found there.
            let colour: f32 = (0..3).map(|ch| (frame[ch * plane + i] - tap.sample(&self.image[ch * plane..])).abs()).sum();
            let found = (tap.sample(mx), tap.sample(my));
            let consistency = (found.0 - c.0).hypot(found.1 - c.1);
            // Slight preference for the simpler candidates on ties.
            let score = colour + 0.02 * consistency + 0.005 * n as f32;
            if score < best.0 {
                best = (score, found);
            }
        }
        best.1
    }
}

/// Tiles that look for nearby foreground motion; with their 3×3 neighbourhood
/// a foreground trail is found up to 1.5 tiles away from its object.
const FG_TILE: usize = 32;
/// Longest foreground trail (half exposure, pixels) spread over neighbours.
const MAX_REACH: f32 = 48.0;

/// Blurs planar RGB (0..1). `ahead` is the motion after this instant and
/// `behind` the motion that led to it, both per source-frame interval;
/// `half_window` is half the exposure, in source-frame intervals.
///
/// Each pixel combines, in linear light:
/// - its own path: the samples along its trail that move with it, plus the
///   background it uncovers when it is in front (soft edges on moving objects);
/// - the trails of nearby objects in front of it that sweep over it, weighted
///   by the fraction of the exposure they cover it.
///
/// Without depth, "in front" is judged against the dominant (camera) motion:
/// what moves unlike it stands out. A crossing enemy stands out from a still
/// background, and a HUD, weapon or followed character stands out from a
/// panning world, so neither ever gets smeared by what is behind it.
pub fn motion_blur(rgb: &[f32], ahead: &[f32], behind: &[f32], width: usize, height: usize, half_window: f32) -> Vec<f32> {
    let plane = width * height;
    let mut linear = vec![0f32; 3 * plane];
    parallel_rows(&mut linear, 3, width, height, |y, row| {
        for (c, out) in row.iter_mut().enumerate() {
            let src = &rgb[c * plane + y * width..][..width];
            for (o, &v) in out.iter_mut().zip(src) {
                *o = to_linear(v);
            }
        }
    });

    let (ax, ay) = ahead.split_at(plane);
    let (bx, by) = behind.split_at(plane);
    // Displacement over half the exposure, averaged over both sides.
    let mut central: Motion = vec![0f32; 2 * plane];
    parallel_rows(&mut central, 2, width, height, |y, row| {
        for (c, out) in row.iter_mut().enumerate() {
            let start = c * plane + y * width;
            for (x, o) in out.iter_mut().enumerate() {
                *o = (ahead[start + x] + behind[start + x]) * 0.5 * half_window;
            }
        }
    });
    let (cx, cy) = central.split_at(plane);
    let dominant = (median(cx), median(cy));
    let mut depth = vec![0f32; plane];
    parallel_rows(&mut depth, 1, width, height, |y, row| {
        for (x, o) in row[0].iter_mut().enumerate() {
            let i = y * width + x;
            *o = (cx[i] - dominant.0).hypot(cy[i] - dominant.1);
        }
    });
    let foreground = ForegroundTiles::new(cx, cy, &depth, width, height);

    let mut out = vec![0f32; 3 * plane];
    parallel_rows(&mut out, 3, width, height, |y, row| {
        for x in 0..width {
            let i = y * width + x;
            let (fwd, back) = ((ax[i], ay[i]), (bx[i], by[i]));
            let trail = half_window * (fwd.0.hypot(fwd.1) + back.0.hypot(back.1));
            let here = depth[i];
            // Only look for sweeping trails when something nearby is in front.
            let (fg_depth, (fg_x, fg_y)) = foreground.at(x, y);
            let reach = if fg_depth > here + 0.5 { fg_x.hypot(fg_y).min(MAX_REACH) } else { 0.0 };
            if trail < 0.5 && reach < 0.5 {
                for (c, out) in row.iter_mut().enumerate() {
                    out[x] = rgb[c * plane + i];
                }
                continue;
            }

            // Own path. The pixel itself always counts, so weights never vanish.
            let mut own = [linear[i], linear[plane + i], linear[2 * plane + i]];
            let mut own_total = 1.0f32;
            if trail >= 0.5 {
                let samples = (trail.ceil() as usize).clamp(2, MAX_SAMPLES);
                for k in 0..samples {
                    // Stratified offsets across the exposure, centred on this frame.
                    let tau = half_window * (2.0 * (k as f32 + 0.5) / samples as f32 - 1.0);
                    let ((vx, vy), (mx, my)) = if tau >= 0.0 { (fwd, (ax, ay)) } else { (back, (bx, by)) };
                    // Where the scene point seen here at time `tau` is now.
                    let tap = Tap::new(width, height, x as f32 - tau * vx, y as f32 - tau * vy);
                    // Tolerates the gradual change of a camera turn (~10% keeps
                    // a weight of 0.9) but not a different object.
                    let sigma = 0.2 * vx.hypot(vy) + 0.5;
                    let inv = 1.0 / (4.0 * sigma * sigma);
                    // Each bilinear corner is judged by its own motion, so a
                    // sample straddling an edge never mixes in the wrong side.
                    for (j, bilinear) in tap.corners() {
                        let (dx, dy) = (mx[j] - vx, my[j] - vy);
                        // A smooth bump that reaches 0 at two sigmas.
                        let q = ((dx * dx + dy * dy) * inv).min(1.0);
                        let same_motion = (1.0 - q) * (1.0 - q);
                        let uncovered = in_front(here, depth[j]);
                        let weight = bilinear * (same_motion + (1.0 - same_motion) * uncovered);
                        if weight < 1e-4 {
                            continue;
                        }
                        for (c, a) in own.iter_mut().enumerate() {
                            *a += weight * linear[c * plane + j];
                        }
                        own_total += weight;
                    }
                }
            }

            // Foreground trails sweeping over this pixel, sampled along the
            // strongest nearby foreground motion.
            let mut cover = 0f32;
            let mut swept = [0f32; 3];
            if reach >= 0.5 {
                let len = fg_x.hypot(fg_y);
                let (dir_x, dir_y) = (fg_x / len, fg_y / len);
                let samples = ((2.0 * reach).ceil() as usize).clamp(2, MAX_SAMPLES);
                let step = 2.0 * reach / samples as f32;
                for k in 0..samples {
                    let s = -reach + step * (k as f32 + 0.5);
                    let (px, py) = ((x as f32 + s * dir_x).round(), (y as f32 + s * dir_y).round());
                    if px < 0.0 || py < 0.0 || px >= width as f32 || py >= height as f32 {
                        continue;
                    }
                    let j = py as usize * width + px as usize;
                    let front = in_front(depth[j], here);
                    let (hx, hy) = (cx[j], cy[j]);
                    let half_trail = hx.hypot(hy);
                    if front == 0.0 || half_trail < 0.5 {
                        continue;
                    }
                    // Does that point's trail pass over this pixel?
                    let (ox, oy) = (x as f32 - px, y as f32 - py);
                    let along = (ox * hx + oy * hy) / half_trail;
                    let across = (ox * hy - oy * hx).abs() / half_trail;
                    if along.abs() > half_trail || across > 1.0 + 0.15 * along.abs() {
                        continue;
                    }
                    // Its colour is spread evenly along its trail.
                    let weight = front * step / (2.0 * half_trail);
                    for (c, s) in swept.iter_mut().enumerate() {
                        *s += weight * linear[c * plane + j];
                    }
                    cover += weight;
                }
            }

            let alpha = cover.min(1.0);
            for (c, out) in row.iter_mut().enumerate() {
                let own = own[c] / own_total;
                let value = if cover > 0.0 { alpha * swept[c] / cover + (1.0 - alpha) * own } else { own };
                out[x] = from_linear(value);
            }
        }
    });
    out
}

/// How clearly a point with depth cue `a` is in front of one with `b`
/// (0 = not, 1 = clearly), tolerant to flow noise.
fn in_front(a: f32, b: f32) -> f32 {
    let scale = 0.3 * a.max(b) + 1.0;
    (2.0 * (a - b) / scale - 1.0).clamp(0.0, 1.0)
}

/// Median of a sparse sample of the field.
fn median(values: &[f32]) -> f32 {
    let mut sample: Vec<f32> = values.iter().step_by(7).copied().collect();
    if sample.is_empty() {
        return 0.0;
    }
    let mid = sample.len() / 2;
    *sample.select_nth_unstable_by(mid, f32::total_cmp).1
}

/// For each tile, the depth cue and half-exposure motion of the most
/// foreground moving point in its 3×3 tile neighbourhood.
struct ForegroundTiles {
    motion: Vec<(f32, (f32, f32))>,
    tiles_x: usize,
}

impl ForegroundTiles {
    fn new(cx: &[f32], cy: &[f32], depth: &[f32], width: usize, height: usize) -> Self {
        let (tiles_x, tiles_y) = (width.div_ceil(FG_TILE), height.div_ceil(FG_TILE));
        let best: Vec<(f32, (f32, f32))> = parallel_map(tiles_y, |ty| {
            let mut band = vec![(f32::NEG_INFINITY, (0f32, 0f32)); tiles_x];
            for y in ty * FG_TILE..((ty + 1) * FG_TILE).min(height) {
                for x in 0..width {
                    let i = y * width + x;
                    // Only moving points that stand out can sweep over others.
                    if cx[i].hypot(cy[i]) < 0.5 || depth[i] < 1.0 {
                        continue;
                    }
                    let tile = &mut band[x / FG_TILE];
                    if depth[i] > tile.0 {
                        *tile = (depth[i], (cx[i], cy[i]));
                    }
                }
            }
            band
        })
        .concat();
        let mut motion = vec![(f32::NEG_INFINITY, (0f32, 0f32)); tiles_x * tiles_y];
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let mut pick = (f32::NEG_INFINITY, (0f32, 0f32));
                for ny in ty.saturating_sub(1)..(ty + 2).min(tiles_y) {
                    for nx in tx.saturating_sub(1)..(tx + 2).min(tiles_x) {
                        let candidate = best[ny * tiles_x + nx];
                        // Deeper in front wins; among equals, the longer trail.
                        let longer = candidate.1.0.hypot(candidate.1.1) > pick.1.0.hypot(pick.1.1);
                        if candidate.0 > pick.0 + 0.5 || (candidate.0 > pick.0 - 0.5 && longer) {
                            pick = candidate;
                        }
                    }
                }
                motion[ty * tiles_x + tx] = pick;
            }
        }
        Self { motion, tiles_x }
    }

    /// Depth cue and half-exposure motion of the strongest foreground nearby
    /// (depth −∞ when there is none).
    fn at(&self, x: usize, y: usize) -> (f32, (f32, f32)) {
        self.motion[(y / FG_TILE) * self.tiles_x + x / FG_TILE]
    }
}

fn to_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(2.2)
}

fn from_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(1.0 / 2.2)
}

/// Bilinear lookup position, shared by all planes sampled at one point.
struct Tap {
    i00: usize,
    i01: usize,
    i10: usize,
    i11: usize,
    fx: f32,
    fy: f32,
}

impl Tap {
    fn new(width: usize, height: usize, x: f32, y: f32) -> Self {
        let x = x.clamp(0.0, (width - 1) as f32);
        let y = y.clamp(0.0, (height - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
        Self {
            i00: y0 * width + x0,
            i01: y0 * width + x1,
            i10: y1 * width + x0,
            i11: y1 * width + x1,
            fx: x - x0 as f32,
            fy: y - y0 as f32,
        }
    }

    /// The four corner indices with their bilinear weights.
    fn corners(&self) -> [(usize, f32); 4] {
        let (fx, fy) = (self.fx, self.fy);
        [
            (self.i00, (1.0 - fx) * (1.0 - fy)),
            (self.i01, fx * (1.0 - fy)),
            (self.i10, (1.0 - fx) * fy),
            (self.i11, fx * fy),
        ]
    }

    fn sample(&self, plane: &[f32]) -> f32 {
        let top = plane[self.i00] + (plane[self.i01] - plane[self.i00]) * self.fx;
        let bottom = plane[self.i10] + (plane[self.i11] - plane[self.i10]) * self.fx;
        top + (bottom - top) * self.fy
    }
}

/// Rows handed to a worker at a time. Small blocks taken on demand keep fast
/// and slow cores (hybrid CPUs) equally busy.
const BLOCK_ROWS: usize = 8;

/// Leaves two hardware threads free: the GPU is fed from the CPU, and with
/// every core busy blurring it would sit idle waiting for work.
fn threads() -> usize {
    thread::available_parallelism().map_or(4, |n| n.get()).saturating_sub(2).max(1)
}

/// Runs `f(y, rows)` for every row on all cores, where `rows` holds that row
/// of each of the `planes` planes of `out`.
fn parallel_rows(out: &mut [f32], planes: usize, width: usize, height: usize, f: impl Fn(usize, &mut [&mut [f32]]) + Sync) {
    let blocks = height.div_ceil(BLOCK_ROWS);
    let mut work: Vec<(usize, Vec<&mut [f32]>)> = (0..blocks).map(|b| (b * BLOCK_ROWS, Vec::with_capacity(planes))).collect();
    for plane in out.chunks_mut(width * height).take(planes) {
        for (b, chunk) in plane.chunks_mut(BLOCK_ROWS * width).enumerate() {
            work[b].1.push(chunk);
        }
    }
    let queue = Mutex::new(work);
    thread::scope(|scope| {
        for _ in 0..threads().min(blocks) {
            scope.spawn(|| {
                loop {
                    // Take the block in its own statement so the lock is
                    // released before the work starts.
                    let next = queue.lock().unwrap().pop();
                    let Some((y0, mut chunks)) = next else { break };
                    let rows = chunks[0].len() / width;
                    for r in 0..rows {
                        let mut row: Vec<&mut [f32]> = chunks.iter_mut().map(|c| &mut c[r * width..(r + 1) * width]).collect();
                        f(y0 + r, &mut row);
                    }
                }
            });
        }
    });
}

/// `(0..n).map(f)` on all cores, in order.
fn parallel_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, T)> = thread::scope(|scope| {
        let workers: Vec<_> = (0..threads().min(n))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break done;
                        }
                        done.push((i, f(i)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    results.sort_unstable_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;
    const H: usize = 16;

    /// A white vertical bar at x = 30..34 on black, as planar RGB.
    fn bar() -> Vec<f32> {
        let mut rgb = vec![0f32; 3 * W * H];
        for c in 0..3 {
            for y in 0..H {
                for x in 30..34 {
                    rgb[c * W * H + y * W + x] = 1.0;
                }
            }
        }
        rgb
    }

    fn uniform(vx: f32) -> Motion {
        let mut m = vec![0f32; 2 * W * H];
        m[..W * H].fill(vx);
        m
    }

    fn row(rgb: &[f32]) -> Vec<f32> {
        rgb[8 * W..9 * W].to_vec()
    }

    #[test]
    fn static_pixels_are_untouched() {
        let rgb = bar();
        let still = uniform(0.0);
        assert_eq!(motion_blur(&rgb, &still, &still, W, H, 0.25), rgb);
    }

    #[test]
    fn moving_content_smears_along_its_motion_and_keeps_energy() {
        // Everything moves 16 px per frame; a 180° shutter at the source rate
        // gives an 8 px trail, centred on the bar.
        let rgb = bar();
        let motion = uniform(16.0);
        let out = row(&motion_blur(&rgb, &motion, &motion, W, H, 0.25));
        let lit: Vec<usize> = (0..W).filter(|&x| out[x] > 0.02).collect();
        assert!(lit.first().unwrap() <= &27 && lit.last().unwrap() >= &36, "{out:?}");
        assert!(out[32] < 0.999, "the bar itself is blurred");
        // Vertical motion would not spread it horizontally.
        assert_eq!(out[10], 0.0);
        // In linear light the total light is preserved.
        let energy = |r: &[f32]| r.iter().map(|&v| to_linear(v)).sum::<f32>();
        assert!((energy(&out) - energy(&row(&rgb))).abs() < 0.6, "{} vs {}", energy(&out), energy(&row(&rgb)));
    }

    #[test]
    fn fast_object_smears_over_still_background() {
        // The bar moves +16 px per frame over a still black background; a
        // 180° exposure moves it 4 px each way, so its trail spans 26..38.
        let rgb = bar();
        let mut motion = uniform(0.0);
        for y in 0..H {
            motion[y * W + 30..y * W + 34].fill(16.0);
        }
        let out = row(&motion_blur(&rgb, &motion, &motion, W, H, 0.25));
        assert!(out[27] > 0.05 && out[36] > 0.05, "smears over the background: {out:?}");
        assert!(out[22] == 0.0 && out[42] == 0.0, "but not beyond its trail: {out:?}");
        assert!(out[31] > 0.5 && out[31] < 0.95, "and turns see-through: {out:?}");
        let energy = |r: &[f32]| r.iter().map(|&v| to_linear(v)).sum::<f32>();
        let (before, after) = (energy(&row(&rgb)), energy(&out));
        assert!((after - before).abs() < 0.25 * before, "light is conserved: {before} vs {after}");
    }

    #[test]
    fn static_overlay_is_not_smeared_by_moving_background() {
        // The bar is a static HUD element; the rest of the picture pans.
        let rgb = bar();
        let mut motion = uniform(16.0);
        for y in 0..H {
            for x in 30..34 {
                motion[y * W + x] = 0.0;
            }
        }
        let out = row(&motion_blur(&rgb, &motion, &motion, W, H, 0.25));
        assert_eq!(&out[30..34], &[1.0; 4], "the overlay stays sharp");
        assert!(out[28] < 0.05 && out[36] < 0.05, "and does not bleed into the background: {out:?}");
    }

    /// Planar RGB with value 1 at columns `from..to` and 0.2 elsewhere.
    fn object(from: usize, to: usize) -> Vec<f32> {
        let mut rgb = vec![0.2f32; 3 * W * H];
        for c in 0..3 {
            for y in 0..H {
                rgb[c * W * H + y * W + from..c * W * H + y * W + to].fill(1.0);
            }
        }
        rgb
    }

    /// RIFE's flow for an object at `from..to` at the midpoint moving `v` px
    /// per pair over a static background.
    fn flow_for(from: usize, to: usize, v: f32) -> Vec<f32> {
        let mut flow = vec![0f32; 4 * W * H];
        for y in 0..H {
            for x in from..to {
                flow[y * W + x] = -v / 2.0; // to A
                flow[2 * W * H + y * W + x] = v / 2.0; // to B
            }
        }
        flow
    }

    #[test]
    fn motion_is_reanchored_in_time() {
        // The object covers 20..30 at the midpoint and moves +10 px per pair,
        // so at tau = 0 it covers 15..25.
        let pair = PairMotion::new(&flow_for(20, 30, 10.0), object(20, 30), W, H);
        let at_a = pair.at(&object(15, 25), 0.0, 1.0);
        let row: Vec<f32> = at_a[8 * W..9 * W].to_vec();
        for x in 15..25 {
            assert_eq!(row[x], 10.0, "object pixel {x} moves: {row:?}");
        }
        for x in (0..14).chain(26..W) {
            assert_eq!(row[x], 0.0, "background pixel {x} is static: {row:?}");
        }
        assert_eq!(pair.at(&object(20, 30), 0.5, 0.5)[8 * W + 25], 5.0);
    }

    #[test]
    fn static_hud_keeps_zero_motion_when_reanchored() {
        // Everything pans +12 px except a static bright HUD block at 30..34.
        let mut flow = vec![0f32; 4 * W * H];
        let mut image = vec![0f32; 3 * W * H];
        for y in 0..H {
            for x in 0..W {
                let hud = (30..34).contains(&x);
                let i = y * W + x;
                if !hud {
                    flow[i] = -6.0;
                    flow[2 * W * H + i] = 6.0;
                }
                for c in 0..3 {
                    image[c * W * H + i] = if hud { 1.0 } else { 0.1 + 0.01 * x as f32 };
                }
            }
        }
        // At tau = 0 the background is shifted 6 px left; the HUD is not.
        let mut frame = vec![0f32; 3 * W * H];
        for y in 0..H {
            for x in 0..W {
                for c in 0..3 {
                    frame[c * W * H + y * W + x] =
                        if (30..34).contains(&x) { 1.0 } else { 0.1 + 0.01 * (x + 6).min(W - 1) as f32 };
                }
            }
        }
        let pair = PairMotion::new(&flow, image, W, H);
        let at_a = pair.at(&frame, 0.0, 1.0);
        for x in 30..34 {
            assert_eq!(at_a[8 * W + x], 0.0, "HUD pixel {x}");
        }
        assert_eq!(at_a[8 * W + 10], 12.0);
    }
}
