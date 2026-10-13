//! Length-prefixed frames: a 4-byte big-endian length, then the payload.
//! Every read takes a hard maximum, so a peer cannot make us allocate more
//! than that no matter what length it claims.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest Noise message (Noise spec §3), and so our largest encrypted frame.
pub const MAX_NOISE_FRAME: usize = 65_535;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Reads one frame of at most `max` bytes. A larger claimed length is an
/// `InvalidData` error and nothing beyond the header is read.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer sent a {len}-byte frame; limit is {max}"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, b"hello").await.unwrap();
        write_frame(&mut a, b"").await.unwrap();
        assert_eq!(read_frame(&mut b, 100).await.unwrap(), b"hello");
        assert_eq!(read_frame(&mut b, 100).await.unwrap(), b"");
    }

    #[tokio::test]
    async fn oversized_claim_is_rejected_without_allocating() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b, MAX_NOISE_FRAME).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&10u32.to_be_bytes()).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        drop(a);
        let err = read_frame(&mut b, 100).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
