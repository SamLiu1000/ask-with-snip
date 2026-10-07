use base64::Engine as _;
use image::{imageops, RgbaImage};

/// A frozen screen frame, plus how it relates to screen coordinates.
///
/// Region selection works as "capture the whole screen first, then crop from the
/// static image", not "select a region first, then capture that area live".
/// Three benefits of doing it this way: the mask window never photographs itself;
/// there is no race or flicker from hiding the mask before capturing; and only a
/// static frame allows feel-oriented refinements like the magnifier and edge snapping.
pub struct Frame {
    pub image: RgbaImage,
    /// Display scale factor (1.0 = 100%)
    pub scale_factor: f32,
    /// Display origin within the virtual desktop, used to map the selection back to screen coordinates
    pub origin_x: i32,
    pub origin_y: i32,
}

impl Frame {
    pub fn width(&self) -> u32 {
        self.image.width()
    }

    pub fn height(&self) -> u32 {
        self.image.height()
    }
}

/// Geometry of a display.
///
/// Keeping this separate from capturing is intentional: capture can fail due to
/// permissions, session isolation and the like, but the display's size and scale
/// factor are always available. On failure we still need this information to place
/// the mask window correctly so the error can be shown to the user.
pub struct MonitorGeometry {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub origin_x: i32,
    pub origin_y: i32,
}

fn pick_primary(monitors: &[xcap::Monitor]) -> Option<&xcap::Monitor> {
    monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
}

/// Reads display geometry only, without capturing.
pub fn primary_geometry() -> Result<MonitorGeometry, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Failed to enumerate monitors: {e}"))?;
    let monitor = pick_primary(&monitors).ok_or("No display detected")?;
    Ok(MonitorGeometry {
        width: monitor.width().unwrap_or(0),
        height: monitor.height().unwrap_or(0),
        scale_factor: monitor.scale_factor().unwrap_or(1.0),
        origin_x: monitor.x().unwrap_or(0),
        origin_y: monitor.y().unwrap_or(0),
    })
}

/// Captures the full screen of the primary display. Only single-monitor setups are
/// supported for now; multi-monitor support is deferred.
pub fn capture_primary() -> Result<Frame, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Failed to enumerate monitors: {e}"))?;
    if monitors.is_empty() {
        return Err("No display detected".into());
    }
    let monitor = pick_primary(&monitors).expect("verified non-empty above");

    let image = monitor.capture_image().map_err(|e| format!("Screen capture failed: {e}"))?;
    if image.width() == 0 || image.height() == 0 {
        return Err("Screen capture returned an empty image".into());
    }

    Ok(Frame {
        image,
        scale_factor: monitor.scale_factor().unwrap_or(1.0),
        origin_x: monitor.x().unwrap_or(0),
        origin_y: monitor.y().unwrap_or(0),
    })
}

/// Crops a piece out of a frozen frame. Coordinates are physical pixels within the
/// frame; out-of-range values are clamped to the image bounds.
pub fn crop(frame: &Frame, x: i32, y: i32, width: u32, height: u32) -> Result<RgbaImage, String> {
    let (fw, fh) = (frame.width(), frame.height());
    let x = x.clamp(0, fw.saturating_sub(1) as i32) as u32;
    let y = y.clamp(0, fh.saturating_sub(1) as i32) as u32;
    let width = width.clamp(1, fw - x);
    let height = height.clamp(1, fh - y);
    Ok(imageops::crop_imm(&frame.image, x, y, width, height).to_image())
}

/// Compresses and encodes to JPEG base64 (without the data: prefix).
///
/// When the long edge exceeds `max_edge`, the image is scaled down proportionally.
/// This is not about saving bandwidth -- vision models downscale images internally
/// anyway; compressing first ourselves merely avoids paying token cost for pixels
/// we would never use.
pub fn encode_jpeg_base64(img: &RgbaImage, max_edge: u32, quality: u8) -> Result<String, String> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("Image to encode has zero dimensions".into());
    }

    let long_edge = w.max(h);
    let scaled = if max_edge > 0 && long_edge > max_edge {
        let ratio = max_edge as f32 / long_edge as f32;
        let nw = ((w as f32 * ratio).round() as u32).max(1);
        let nh = ((h as f32 * ratio).round() as u32).max(1);
        imageops::resize(img, nw, nh, imageops::FilterType::Lanczos3)
    } else {
        img.clone()
    };

    // JPEG does not accept an alpha channel; convert to RGB8 before encoding.
    let rgb = image::DynamicImage::ImageRgba8(scaled).to_rgb8();
    let mut buf: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode_image(&rgb)
        .map_err(|e| format!("JPEG encoding failed: {e}"))?;

    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}

/// A data URL the frontend can feed directly into an <img>. The region-selection
/// background image goes through this path.
pub fn to_data_url_jpeg(img: &RgbaImage, max_edge: u32, quality: u8) -> Result<String, String> {
    Ok(format!(
        "data:image/jpeg;base64,{}",
        encode_jpeg_base64(img, max_edge, quality)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([12, 34, 56, 255]))
    }

    #[test]
    fn crop_clamps_to_frame_bounds() {
        let frame = Frame {
            image: solid(100, 50),
            scale_factor: 1.0,
            origin_x: 0,
            origin_y: 0,
        };

        let ok = crop(&frame, 10, 10, 20, 20).unwrap();
        assert_eq!((ok.width(), ok.height()), (20, 20));

        // Bottom-right corner out of range: should be clipped to the image bounds, not panic
        let clipped = crop(&frame, 90, 40, 100, 100).unwrap();
        assert_eq!((clipped.width(), clipped.height()), (10, 10));

        // A fully-outside origin gets clamped to the last pixel
        let outside = crop(&frame, 500, 500, 10, 10).unwrap();
        assert_eq!((outside.width(), outside.height()), (1, 1));
    }

    #[test]
    fn encode_downscales_only_when_needed() {
        let img = solid(4000, 2000);
        let b64 = encode_jpeg_base64(&img, 1568, 80).unwrap();
        assert!(!b64.is_empty());

        // When the long edge is already within the limit, it should not be upscaled
        let small = solid(100, 80);
        let b64 = encode_jpeg_base64(&small, 1568, 80).unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let decoded = image::load_from_memory(&raw).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (100, 80));
    }

    #[test]
    fn encode_rejects_empty_image() {
        let empty = RgbaImage::new(0, 0);
        assert!(encode_jpeg_base64(&empty, 1568, 80).is_err());
    }
}
