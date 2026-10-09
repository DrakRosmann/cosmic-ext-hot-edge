// SPDX-License-Identifier: GPL-3.0-only

//! The animation where the edge is pushed: GNOME Shell's ripples
//! (`js/ui/ripples.js`), three translucent white circles growing and fading,
//! with Hot Edge's `ripple-centered` style (104 px wide, white at 20%).

/// The ripple's radius at scale 1, in logical pixels.
const RADIUS: f64 = 52.0;
/// The soft glow around it (`box-shadow: 0 0 2px 2px`).
const GLOW: f64 = 4.0;
const ALPHA: f64 = 0.2;

struct Wave {
    delay: f64,
    duration: f64,
    start_scale: f64,
    start_opacity: f64,
    final_scale: f64,
}

/// `playAnimation`'s three ripples, in milliseconds.
const WAVES: [Wave; 3] = [
    Wave { delay: 0.0, duration: 830.0, start_scale: 0.25, start_opacity: 1.0, final_scale: 1.5 },
    Wave { delay: 50.0, duration: 1000.0, start_scale: 0.0, start_opacity: 0.7, final_scale: 1.25 },
    Wave { delay: 350.0, duration: 1000.0, start_scale: 0.0, start_opacity: 0.3, final_scale: 1.0 },
];

pub const DURATION_MS: f64 = 1350.0;

/// How far the largest ripple reaches from its center, in logical pixels.
pub const EXTENT: u32 = 82;

/// Draws the ripples `t` milliseconds in, centered at `center` (logical
/// pixels), into an ARGB8888 `canvas` of `width`×`height` pixels at `scale`.
pub fn draw(canvas: &mut [u8], width: u32, height: u32, scale: f64, center: (f64, f64), t: f64) {
    // Each ripple's radius, glow and opacity now.
    let waves: Vec<(f64, f64, f64)> = WAVES
        .iter()
        .filter_map(|wave| {
            let progress = ((t - wave.delay) / wave.duration).clamp(0.0, 1.0);
            let size = wave.start_scale + (wave.final_scale - wave.start_scale) * progress;
            // Opacity eases in quadratically; Clutter's opacity is 0–255 and
            // GNOME starts it at sqrt(start_opacity).
            let opacity = wave.start_opacity.sqrt() * (1.0 - progress * progress);
            (size > 0.0 && opacity > 0.0).then_some((RADIUS * size, GLOW * size, opacity))
        })
        .collect();

    for (index, pixel) in canvas.chunks_exact_mut(4).enumerate() {
        let x = (index as u32 % width) as f64;
        let y = (index as u32 / width) as f64;
        if y >= height as f64 {
            break;
        }
        let dx = (x + 0.5) / scale - center.0;
        let dy = (y + 0.5) / scale - center.1;
        let distance = (dx * dx + dy * dy).sqrt();

        let mut alpha = 0.0;
        for &(radius, glow, opacity) in &waves {
            // Solid inside, a pixel of antialiasing, then the glow fading out.
            let coverage = if distance <= radius {
                1.0
            } else if glow > 0.0 && distance < radius + glow {
                1.0 - (distance - radius) / glow
            } else {
                0.0
            };
            let a = coverage * ALPHA * opacity;
            alpha = a + alpha * (1.0 - a);
        }
        // White, premultiplied.
        let value = (alpha * 255.0).round() as u8;
        pixel.copy_from_slice(&[value, value, value, value]);
    }
}
