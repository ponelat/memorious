//! One stored format per media type: JPEG photos, AAC/m4a audio.
//! Photos are re-encoded here (also strips metadata). Audio that isn't already an
//! MP4/M4A container must be transcoded by the caller (ffmpeg on the server) —
//! core only detects, it doesn't shell out.

use anyhow::{Context, Result};

pub const JPEG_QUALITY: u8 = 85;

/// Decode any supported image format and re-encode as JPEG. No resize; EXIF dropped.
pub fn normalize_photo(bytes: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes).context("decode image")?;
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
