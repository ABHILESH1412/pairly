//! Length-prefixed framing: `[u16 BE length][bytes]`. One frame carries one Noise message,
//! so frames are at most 65535 bytes.
//!
//! [`read_frame`] is not cancel-safe; drive it from a dedicated reader task, not inside `select!`.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_LEN: usize = u16::MAX as usize;

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Write one frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, data: &[u8]) -> io::Result<()> {
    let len = u16::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds 65535 bytes"))?;
    let mut buf = Vec::with_capacity(2 + data.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_frame(&mut a, b"hello").await.unwrap();
        write_frame(&mut a, &[]).await.unwrap();
        write_frame(&mut a, &vec![9; MAX_FRAME_LEN]).await.unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"");
        assert_eq!(
            read_frame(&mut b).await.unwrap().unwrap().len(),
            MAX_FRAME_LEN
        );
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected() {
        let (mut a, _b) = tokio::io::duplex(16);
        let err = write_frame(&mut a, &vec![0; MAX_FRAME_LEN + 1])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 10, 1, 2, 3]).await.unwrap();
        drop(a);
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
