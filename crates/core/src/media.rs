//! One stored format per media type: JPEG photos, AAC/m4a audio.
//! Photos are re-encoded here (also strips metadata). Audio that isn't already an
//! MP4/M4A container must be transcoded by the caller (ffmpeg on the server) —
//! core only detects, it doesn't shell out.

use anyhow::{Context, Result};

pub const JPEG_QUALITY: u8 = 85;

/// Decode any supported image format and re-encode as JPEG. No resize; EXIF
/// dropped — which is exactly why the orientation flag must be *applied* to
/// the pixels first: cameras store portrait shots as landscape pixels plus a
/// "rotate to view" tag, and stripping the tag without rotating shows every
/// portrait photo landscape.
pub fn normalize_photo(bytes: &[u8]) -> Result<Vec<u8>> {
    use image::ImageDecoder;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .context("sniff image format")?;
    let mut decoder = reader.into_decoder().context("decode image")?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).context("decode image")?;
    img.apply_orientation(orientation);
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode_image(&img).context("encode jpeg")?;
    Ok(out)
}

/// True if the bytes look like an MP4-family container (m4a/mp4/mov): `....ftyp`.
pub fn is_mp4_family(bytes: &[u8]) -> bool {
    bytes.len() > 12 && &bytes[4..8] == b"ftyp"
}

/// Best-effort container sniff for audio uploads, for error messages and transcode
/// decisions.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AudioContainer {
    Mp4,
    Webm,
    Ogg,
    Unknown,
}

pub fn sniff_audio(bytes: &[u8]) -> AudioContainer {
    if is_mp4_family(bytes) {
        AudioContainer::Mp4
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        AudioContainer::Webm
    } else if bytes.starts_with(b"OggS") {
        AudioContainer::Ogg
    } else {
        AudioContainer::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photos_normalize_to_jpeg() {
        // 4x4 red PNG, made in-memory.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            4,
            4,
            image::Rgb([255, 0, 0]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();

        let jpeg = normalize_photo(png.get_ref()).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let round = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((round.width(), round.height()), (4, 4));

        // Garbage is rejected.
        assert!(normalize_photo(b"not an image").is_err());
    }

    /// A JPEG the way a phone camera writes portrait shots: landscape pixels
    /// plus an EXIF orientation tag ("rotate 90° CW to view"). Built by
    /// splicing a minimal EXIF APP1 segment right after SOI.
    fn portrait_jpeg_4x2_left_red() -> Vec<u8> {
        let mut img = image::RgbImage::from_pixel(4, 2, image::Rgb([0, 0, 255]));
        for y in 0..2 {
            for x in 0..2 {
                img.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();

        // Minimal EXIF: TIFF header + one IFD entry, tag 0x0112 = 6 (Rotate90).
        let mut exif: Vec<u8> = Vec::new();
        exif.extend_from_slice(b"Exif\0\0");
        exif.extend_from_slice(b"II\x2a\0\x08\0\0\0"); // little-endian TIFF, IFD at 8
        exif.extend_from_slice(&1u16.to_le_bytes()); // one entry
        exif.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        exif.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        exif.extend_from_slice(&1u32.to_le_bytes()); // count
        exif.extend_from_slice(&6u32.to_le_bytes()); // value 6, padded
        exif.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);

        let mut out = jpeg[..2].to_vec(); // SOI
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn normalize_applies_exif_orientation() {
        // The camera's "portrait" flag must become rotated pixels — the flag
        // itself is stripped with the rest of the metadata, so without this
        // the photo renders landscape everywhere ("shot portrait, shown
        // landscape").
        let jpeg = normalize_photo(&portrait_jpeg_4x2_left_red()).unwrap();
        let round = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!((round.width(), round.height()), (2, 4), "dimensions rotated");
        let top = round.get_pixel(0, 0);
        let bottom = round.get_pixel(0, 3);
        assert!(top[0] > 180 && top[2] < 80, "red edge on top after 90° CW: {top:?}");
        assert!(bottom[2] > 180 && bottom[0] < 80, "blue edge at bottom: {bottom:?}");
    }

    #[test]
    fn audio_container_sniffing() {
        let mut m4a = vec![0, 0, 0, 24];
        m4a.extend_from_slice(b"ftypM4A ");
        m4a.extend_from_slice(&[0; 8]);
        assert_eq!(sniff_audio(&m4a), AudioContainer::Mp4);
        assert!(is_mp4_family(&m4a));

        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3];
        webm.extend_from_slice(&[0; 16]);
        assert_eq!(sniff_audio(&webm), AudioContainer::Webm);
        assert_eq!(sniff_audio(b"OggS plus more bytes here"), AudioContainer::Ogg);
        assert_eq!(sniff_audio(b"???"), AudioContainer::Unknown);
    }
}
