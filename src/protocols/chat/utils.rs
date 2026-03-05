use bytes::{Bytes, BytesMut};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::message::ProtoMessage;

/// Errors related to message writing
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Serialization failed
    #[error(transparent)]
    Ser(#[from] postcard::Error),
    /// IO error
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Message was larger than the configured maximum message size
    #[error("message too large")]
    TooLarge,
}

/// Write a `ProtoMessage` as a length-prefixed, postcard-encoded message.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    buffer: &mut BytesMut,
    frame: &ProtoMessage,
    max_message_size: usize,
) -> Result<(), WriteError> {
    let len = postcard::experimental::serialized_size(&frame)?;
    if len >= max_message_size {
        return Err(WriteError::TooLarge);
    }

    buffer.clear();
    buffer.resize(len, 0u8);
    let slice = postcard::to_slice(&frame, buffer)?;
    writer.write_u32(len as u32).await?;
    writer.write_all(slice).await?;
    Ok(())
}

/// Errors related to message reading
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Deserialization failed
    #[error(transparent)]
    De(#[from] postcard::Error),
    /// IO error
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Message was larger than the configured maximum message size
    #[error("message too large")]
    TooLarge,
}

/// Read a length-prefixed message and decode as `ProtoMessage`;
pub async fn read_message(
    reader: impl AsyncRead + Unpin,
    buffer: &mut BytesMut,
    max_message_size: usize,
) -> Result<Option<ProtoMessage>, ReadError> {
    match read_lp(reader, buffer, max_message_size).await? {
        None => Ok(None),
        Some(data) => {
            let message = postcard::from_bytes(&data)?;
            Ok(Some(message))
        }
    }
}

/// Reads a length prefixed message.
///
/// # Returns
///
/// The message as raw bytes.  If the end of the stream is reached and there is no partial
/// message, returns `None`.
pub async fn read_lp(
    mut reader: impl AsyncRead + Unpin,
    buffer: &mut BytesMut,
    max_message_size: usize,
) -> Result<Option<Bytes>, ReadError> {
    let size = match reader.read_u32().await {
        Ok(size) => size,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut reader = reader.take(size as u64);
    let size = usize::try_from(size).map_err(|_| ReadError::TooLarge)?;
    if size > max_message_size {
        return Err(ReadError::TooLarge);
    }
    buffer.reserve(size);
    loop {
        let r = reader.read_buf(buffer).await?;
        if r == 0 {
            break;
        }
    }
    Ok(Some(buffer.split_to(size).freeze()))
}
