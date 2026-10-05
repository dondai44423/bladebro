//! Bounded, cancellation-safe framing for the stdio/socket/pipe boundaries.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Append through one delimiter (inclusive), retaining partial bytes across
/// cancellation. The caller clears `buffer` only after consuming a frame.
pub(crate) async fn read_until_limited<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    delimiter: u8,
    buffer: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buffer.len());
        }
        let end = available.iter().position(|&b| b == delimiter);
        let count = end.map_or(available.len(), |i| i + 1);
        if count > limit.saturating_sub(buffer.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message exceeds size limit",
            ));
        }
        buffer.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(buffer.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn framing_preserves_following_frames_and_enforces_limit_before_eof() {
        let mut reader = BufReader::with_capacity(2, &b"abc\nnext\n"[..]);
        let mut buffer = Vec::new();
        assert_eq!(
            read_until_limited(&mut reader, b'\n', &mut buffer, 4)
                .await
                .unwrap(),
            4
        );
        assert_eq!(buffer, b"abc\n");
        buffer.clear();
        assert_eq!(
            read_until_limited(&mut reader, b'\n', &mut buffer, 5)
                .await
                .unwrap(),
            5
        );
        assert_eq!(buffer, b"next\n");
        let (mut writer, input) = tokio::io::duplex(16);
        writer.write_all(b"too-long").await.unwrap();
        let mut reader = BufReader::new(input);
        buffer.clear();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            read_until_limited(&mut reader, 0, &mut buffer, 4),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert!(buffer.len() <= 4);
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_utf8_and_final_eof_frame() {
        let (mut writer, input) = tokio::io::duplex(16);
        let mut reader = BufReader::new(input);
        let mut buffer = Vec::new();
        writer.write_all(&[0xf0, 0x9f]).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            read_until_limited(&mut reader, b'\n', &mut buffer, 10)
        )
        .await
        .is_err());
        writer.write_all(&[0xa6, 0x80]).await.unwrap();
        drop(writer);
        assert_eq!(
            read_until_limited(&mut reader, b'\n', &mut buffer, 10)
                .await
                .unwrap(),
            4
        );
        assert_eq!(String::from_utf8(buffer).unwrap(), "🦀");
    }
}
