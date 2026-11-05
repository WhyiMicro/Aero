// The Aero icon: a red "record" dot with a white ring (grey while paused).
// Shared with build.rs (via include!), which turns it into the .exe's icon.

/// RGBA pixels of a `size`x`size` icon.
pub fn icon_rgba(size: usize, recording: bool) -> Vec<u8> {
    let fill = if recording { [229u8, 57, 53] } else { [140, 140, 140] };
    let mut rgba = vec![0u8; size * size * 4];
    let c = (size as f32 - 1.0) / 2.0;
    let scale = size as f32 / 32.0;
    let (outer_r, inner_r) = (15.0 * scale, 12.5 * scale);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let outer = (outer_r - d).clamp(0.0, 1.0); // anti-aliased edges
            let inner = (inner_r - d).clamp(0.0, 1.0);
            let rgb = fill.map(|f| (f as f32 * inner + 255.0 * (1.0 - inner)) as u8);
            let i = (y * size + x) * 4;
            rgba[i..i + 3].copy_from_slice(&rgb);
            rgba[i + 3] = (outer * 255.0) as u8;
        }
    }
    rgba
}
