use crate::adapter::successful_results;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use spice_framework::AgentOutput;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated PNG integer")?
            .try_into()?,
    ))
}
/// Validate the actual PNG transport: signature, chunk bounds/CRCs, IHDR,
/// nonempty image data, IEND, and dimensions. This is not a visual-quality score.
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() > 8 * 1024 * 1024 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("missing PNG signature or image exceeds 8 MiB");
    }
    let mut offset = 8;
    let mut dimensions = None;
    let mut image_data = false;
    let mut ended = false;
    while offset < bytes.len() {
        let length = u32_at(bytes, offset)? as usize;
        let end = offset
            .checked_add(length)
            .and_then(|n| n.checked_add(12))
            .context("PNG length overflow")?;
        if end > bytes.len() {
            bail!("PNG chunk extends beyond actual data");
        }
        let kind = &bytes[offset + 4..offset + 8];
        let expected_crc = u32_at(bytes, end - 4)?;
        if crc32(&bytes[offset + 4..end - 4]) != expected_crc {
            bail!("PNG chunk CRC mismatch");
        }
        if offset == 8 && kind != b"IHDR" {
            bail!("PNG first chunk is not IHDR");
        }
        if kind == b"IHDR" {
            if length != 13 || dimensions.is_some() {
                bail!("invalid PNG IHDR");
            }
            let width = u32_at(bytes, offset + 8)?;
            let height = u32_at(bytes, offset + 12)?;
            if !(320..=1920).contains(&width) || !(240..=1600).contains(&height) {
                bail!("PNG dimensions outside preview bounds: {width}x{height}");
            }
            dimensions = Some((width, height));
        } else if kind == b"IDAT" && length > 0 {
            image_data = true;
        } else if kind == b"IEND" {
            if length != 0 || end != bytes.len() {
                bail!("PNG invalid IEND or trailing bytes");
            }
            ended = true;
        }
        offset = end;
    }
    if !ended || !image_data {
        bail!("PNG has no image data or IEND");
    }
    dimensions.context("PNG has no IHDR")
}

fn validate_result(arguments: &Value, result: &Value) -> Result<Vec<Vec<u8>>> {
    let width = arguments["width"].as_u64().unwrap_or(1440);
    let height = arguments["height"].as_u64().unwrap_or(1000);
    let mut images = vec![];
    for block in result["content"]
        .as_array()
        .context("preview content is not blocks")?
    {
        if block["type"] != "image" {
            continue;
        }
        if block["mimeType"] != "image/png" {
            bail!("preview image is not image/png");
        }
        let data = block["data"]
            .as_str()
            .context("preview image missing base64 data")?;
        if data.len() > 12 * 1024 * 1024 {
            bail!("preview encoded data exceeds limit");
        }
        let bytes = STANDARD
            .decode(data)
            .context("preview image is not valid base64")?;
        let actual = dimensions(&bytes)?;
        if actual != (width as u32, height as u32) {
            bail!("PNG dimensions {actual:?} differ from requested {width}x{height}");
        }
        images.push(bytes);
    }
    if images.is_empty() {
        bail!("successful render_preview returned no actual PNG image block");
    }
    Ok(images)
}

pub fn expect_preview(output: &AgentOutput) -> Result<(), String> {
    let results = successful_results(output, "render_preview");
    if results.is_empty() {
        return Err(
            "no successful actual render_preview; browser absence is a failure, not a skip".into(),
        );
    }
    for (call, result) in results {
        validate_result(&call.arguments, &result).map_err(|error| error.to_string())?;
    }
    Ok(())
}
pub fn save_previews(output: &AgentOutput, directory: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![];
    for (call, result) in successful_results(output, "render_preview") {
        for bytes in validate_result(&call.arguments, &result)? {
            let path = directory.join(format!("preview-{}.png", paths.len() + 1));
            fs::write(&path, bytes)?;
            paths.push(path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_text_and_truncated_images_without_external_resources() {
        assert!(dimensions(b"not a screenshot").is_err());
        assert!(dimensions(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(dimensions(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR").is_err());
    }
}
