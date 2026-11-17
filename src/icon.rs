//! Procedurally drawn tray icons.
//!
//! Three states share one glyph whose colour carries the meaning, so the icon
//! stays recognisable in a crowded notification area.

use tray_icon::Icon;

const SIZE: u32 = 64;

pub const GRAY: [u8; 3] = [140, 140, 140];
pub const GREEN: [u8; 3] = [46, 160, 67];
pub const AMBER: [u8; 3] = [214, 150, 0];
pub const BLUE: [u8; 3] = [66, 133, 244];

/// A filled disc with a slightly brighter core and one pixel of anti-aliasing.
pub fn disc(rgb: [u8; 3]) -> Icon {
    let size = SIZE;
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.45;
    let core = radius * 0.52;

    let mut buffer = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            // 1px coverage ramp on the outer edge.
            let coverage = (radius - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                buffer.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let color = if distance < core {
                lighten(rgb, 0.35)
            } else {
                rgb
            };
            buffer.extend_from_slice(&[
                color[0],
                color[1],
                color[2],
                (coverage * 255.0) as u8,
            ]);
        }
    }

    Icon::from_rgba(buffer, size, size).expect("icon buffer is valid RGBA")
}

fn lighten(rgb: [u8; 3], amount: f32) -> [u8; 3] {
    [
        (rgb[0] as f32 + (255.0 - rgb[0] as f32) * amount) as u8,
        (rgb[1] as f32 + (255.0 - rgb[1] as f32) * amount) as u8,
        (rgb[2] as f32 + (255.0 - rgb[2] as f32) * amount) as u8,
    ]
}
