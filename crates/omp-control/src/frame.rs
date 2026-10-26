//! Installed OMP JSONL v2 framing. Limits match the engine's rpc-frame.ts.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub struct FrameDecoder {
    pending: Option<Chunks>,
}
struct Chunks {
    id: String,
    count: usize,
    length: usize,
    next: usize,
    bytes: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, frame: Value) -> Result<Option<Value>> {
        if !frame.is_object() {
            bail!("RPC frame must be an object");
        }
        if frame.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            if self.pending.is_some() {
                bail!("RPC chunk sequence interrupted");
            }
            return Ok(Some(frame));
        }
        let id = frame
            .get("chunkId")
            .and_then(Value::as_str)
            .context("Invalid RPC chunk id")?;
        let number = |key| -> Result<usize> {
            let value = frame
                .get(key)
                .and_then(Value::as_u64)
                .context("Invalid RPC chunk metadata")?;
            usize::try_from(value).context("RPC chunk metadata overflow")
        };
        let index = number("index")?;
        let count = number("count")?;
        let length = number("byteLength")?;
        if id.is_empty()
            || id.len() > 128
            || !(2..=MAX_REASSEMBLED_BYTES / CHUNK_BYTES).contains(&count)
            || index >= count
            || !(MAX_FRAME_BYTES..=MAX_REASSEMBLED_BYTES).contains(&length)
        {
            bail!("Invalid RPC chunk metadata");
        }
        let data = frame
            .get("data")
            .and_then(Value::as_str)
            .context("Invalid RPC chunk data")?;
        if data.is_empty() || data.len() > (CHUNK_BYTES + 2) / 3 * 4 {
            bail!("Invalid RPC chunk payload size");
        }
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| anyhow::anyhow!("Invalid RPC chunk base64"))?;
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            bail!("Invalid RPC chunk payload size");
        }
        if self.pending.is_none() {
            if index != 0 {
                bail!("RPC chunks must start at index zero");
            }
            self.pending = Some(Chunks {
                id: id.to_owned(),
                count,
                length,
                next: 0,
                bytes: Vec::with_capacity(length),
            });
        }
        let pending = self.pending.as_mut().unwrap();
        if pending.id != id
            || pending.count != count
            || pending.length != length
            || pending.next != index
        {
            bail!("RPC chunk sequence mismatch");
        }
        if pending.bytes.len() + bytes.len() > length {
            bail!("RPC chunks exceed declared length");
        }
        pending.bytes.extend_from_slice(&bytes);
        pending.next += 1;
        if pending.next < count {
            return Ok(None);
        }
        let pending = self.pending.take().unwrap();
        if pending.bytes.len() != length {
            bail!("RPC chunk length mismatch");
        }
        let frame: Value = serde_json::from_slice(&pending.bytes)
            .map_err(|_| anyhow::anyhow!("Invalid reassembled RPC JSON"))?;
        if !frame.is_object() || frame.get("type").and_then(Value::as_str) == Some("rpc_chunk") {
            bail!("Invalid reassembled RPC frame");
        }
        Ok(Some(frame))
    }

    pub fn finish(&self) -> Result<()> {
        if self.pending.is_some() {
            bail!("RPC output ended inside a chunk sequence");
        }
        Ok(())
    }
}

/// Reads without read_until's unbounded allocation. EOF inside a line is truncation,
/// not a successful final frame; diagnostics never include the engine's payload.
pub async fn read_json_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<Option<Value>> {
    line.clear();
    loop {
        let bytes = reader.fill_buf().await.context("Cannot read OMP output")?;
        if bytes.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("OMP output ended inside a JSONL frame");
        }
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(bytes.len(), |index| index + 1);
        if line.len() + count > MAX_FRAME_BYTES {
            bail!("OMP JSONL frame exceeds transport limit");
        }
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if newline.is_some() {
            if line.iter().all(u8::is_ascii_whitespace) {
                line.clear();
                continue;
            }
            return serde_json::from_slice(line)
                .map(Some)
                .map_err(|_| anyhow::anyhow!("OMP emitted invalid JSONL"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    fn chunk(id: &str, index: usize, count: usize, length: usize, data: &[u8]) -> Value {
        json!({"type":"rpc_chunk","chunkId":id,"index":index,"count":count,"byteLength":length,"data":STANDARD.encode(data)})
    }

    #[test]
    fn reassembles_large_utf8_at_arbitrary_boundaries() {
        let value =
            json!({"type":"message_end","message":{"content":"🦀".repeat(MAX_FRAME_BYTES / 4)}});
        let bytes = serde_json::to_vec(&value).unwrap();
        let count = bytes.len().div_ceil(CHUNK_BYTES);
        let mut decoder = FrameDecoder::default();
        for (index, data) in bytes.chunks(CHUNK_BYTES).enumerate() {
            let result = decoder
                .push(chunk("utf8", index, count, bytes.len(), data))
                .unwrap();
            if index + 1 == count {
                assert_eq!(result, Some(value.clone()));
            } else {
                assert!(result.is_none());
            }
        }
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_out_of_order_interrupted_and_truncated_sequences() {
        let mut decoder = FrameDecoder::default();
        assert!(decoder
            .push(chunk("a", 1, 4, MAX_FRAME_BYTES, b"x"))
            .is_err());
        decoder
            .push(chunk("a", 0, 4, MAX_FRAME_BYTES, b"x"))
            .unwrap();
        assert!(decoder.finish().is_err());
        assert!(decoder.push(json!({"type":"response"})).is_err());
        assert!(decoder
            .push(chunk("a", 2, 4, MAX_FRAME_BYTES, b"x"))
            .is_err());
    }

    #[test]
    fn rejects_invalid_base64_and_oversized_metadata() {
        let mut decoder = FrameDecoder::default();
        let mut frame = chunk("a", 0, 4, MAX_FRAME_BYTES, b"x");
        frame["data"] = json!("====");
        assert!(decoder.push(frame).is_err());
        assert!(decoder
            .push(chunk("a", 0, 4, MAX_REASSEMBLED_BYTES + 1, b"x"))
            .is_err());
        assert!(decoder.push(json!([])).is_err());
    }

    #[tokio::test]
    async fn handles_split_lines_crlf_and_multiple_frames() {
        let input = b"\r\n{\"type\":\"ready\"}\r\n{\"type\":\"response\"}\n";
        let mut reader = BufReader::with_capacity(3, &input[..]);
        let mut line = Vec::new();
        assert_eq!(
            read_json_line(&mut reader, &mut line)
                .await
                .unwrap()
                .unwrap()["type"],
            "ready"
        );
        assert_eq!(
            read_json_line(&mut reader, &mut line)
                .await
                .unwrap()
                .unwrap()["type"],
            "response"
        );
        assert!(read_json_line(&mut reader, &mut line)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn rejects_truncated_and_oversized_lines_without_payload_diagnostics() {
        let mut line = Vec::new();
        let mut reader = BufReader::new(&b"{\"secret\":true}"[..]);
        let error = read_json_line(&mut reader, &mut line).await.unwrap_err();
        assert!(!error.to_string().contains("secret"));
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(
            read_json_line(&mut BufReader::new(bytes.as_slice()), &mut line)
                .await
                .is_err()
        );
    }
}
