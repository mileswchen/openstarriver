//! LSP wire transport: JSON-RPC 2.0 over stdio using Content-Length framing.
//!
//! Unlike MCP's newline-delimited stdio transport, LSP frames carry a
//! `Content-Length: <n>\r\n\r\n` header followed by exactly `<n>` body bytes.
//! The server also pushes notifications unprompted, so a response to one
//! request may be interleaved with unrelated frames — callers must pump the
//! stream until the matching id arrives (see `client::LspClient`).

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one Content-Length framed message body from `r`.
pub async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            bail!("eof while reading headers");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse()?);
        }
    }
    let len = content_length.context("no Content-Length header")?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one JSON-RPC message to `w` as a Content-Length framed frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_write_frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "m", "params": { "a": [1, 2] } });
        write_frame(&mut a, &msg).await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut b);
        let body = read_frame(&mut reader).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), msg);
    }

    #[tokio::test]
    async fn write_frame_emits_content_length_header() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "m" });
        write_frame(&mut a, &msg).await.unwrap();

        let expected_len = serde_json::to_vec(&msg).unwrap().len();
        let mut reader = tokio::io::BufReader::new(&mut b);
        // Inspect the frame by hand: header line, blank line, then exactly the body.
        let mut first = String::new();
        reader.read_line(&mut first).await.unwrap();
        assert_eq!(first, format!("Content-Length: {}\r\n", expected_len));
        let mut blank = String::new();
        reader.read_line(&mut blank).await.unwrap();
        assert_eq!(blank, "\r\n");
        let mut body = vec![0u8; expected_len];
        reader.read_exact(&mut body).await.unwrap();
        assert_eq!(body, serde_json::to_vec(&msg).unwrap());
    }

    #[tokio::test]
    async fn read_frame_handles_empty_body() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let msg = serde_json::json!({});
        write_frame(&mut a, &msg).await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut b);
        let body = read_frame(&mut reader).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            serde_json::json!({})
        );
    }

    #[tokio::test]
    async fn read_frame_bails_on_eof() {
        let (a, b) = tokio::io::duplex(1 << 16);
        drop(a);
        let mut reader = tokio::io::BufReader::new(b);
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn read_frame_bails_without_content_length() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        a.write_all(b"nonsense\r\n\r\n{}").await.unwrap();
        let mut reader = tokio::io::BufReader::new(&mut b);
        assert!(read_frame(&mut reader).await.is_err());
    }
}
