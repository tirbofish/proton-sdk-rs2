use crate::pgp::PgpPrivateKey;
use base64::{Engine, engine::general_purpose::STANDARD};
use proton_rpgp::{KeyGenerationType, KeyGenerator};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const AEAD_CHUNK_LENGTH: u64 = 1 << 17; // 128 KiB

const PASSPHRASE_RANDOM_BYTES_LENGTH: usize = 32;
const FOLDER_HASH_KEY_LENGTH: usize = 32;

pub struct CryptoGenerator;

impl CryptoGenerator {
    /// Generates a base64-encoded passphrase from 32 random bytes.
    pub fn generate_passphrase() -> String {
        let mut random_bytes = [0u8; PASSPHRASE_RANDOM_BYTES_LENGTH];
        rand::rng().fill(&mut random_bytes);
        STANDARD.encode(random_bytes)
    }

    /// Generates a new PGP private key. Mirrors `PgpPrivateKey.Generate(...)`.
    pub fn generate_private_key() -> anyhow::Result<PgpPrivateKey> {
        KeyGenerator::default()
            .with_key_type(KeyGenerationType::ECC)
            .with_user_id("Drive key", "no-reply@proton.me")
            .generate()
            .map(PgpPrivateKey)
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Generates a new PGP session key.
    pub fn generate_session_key() -> crate::pgp::PgpSessionKey {
        let mut key = vec![0u8; 32];
        rand::rng().fill(&mut key[..]);
        crate::pgp::PgpSessionKey {
            algorithm: 9, // AES256
            key,
        }
    }

    /// Generates a new folder hash key.
    pub fn generate_folder_hash_key() -> Vec<u8> {
        let mut key = vec![0u8; FOLDER_HASH_KEY_LENGTH];
        rand::rng().fill(&mut key[..]);
        key
    }
}

/// Wraps a `Read` and feeds all read bytes into a `sha2::Sha256` hasher.
pub struct HashingReadStream<R: Read> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReadStream<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        self.hasher.finalize().to_vec()
    }
}

impl<R: Read> Read for HashingReadStream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.inner.read(buf)?;
        if bytes_read > 0 {
            self.hasher.update(&buf[..bytes_read]);
        }
        Ok(bytes_read)
    }
}

/// Wraps a `Write` and feeds all written bytes into a `sha2::Sha256` hasher.
pub struct HashingWriteStream<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriteStream<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        self.hasher.finalize().to_vec()
    }
}

impl<W: Write> Write for HashingWriteStream<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes_written = self.inner.write(buf)?;
        if bytes_written > 0 {
            self.hasher.update(&buf[..bytes_written]);
        }
        Ok(bytes_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A wrapper for an `AsyncRead` that also computes a SHA256 hash.
pub struct AsyncHashingReadStream<R: AsyncRead + Unpin> {
    inner: R,
    hasher: Sha256,
}

impl<R: AsyncRead + Unpin> AsyncHashingReadStream<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        self.hasher.finalize().to_vec()
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AsyncHashingReadStream<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &result {
            let after = buf.filled().len();
            if after > before {
                self.hasher.update(&buf.filled()[before..after]);
            }
        }
        result
    }
}

/// A wrapper for an `AsyncWrite` that also computes a SHA256 hash.
pub struct AsyncHashingWriteStream<W: AsyncWrite + Unpin> {
    inner: W,
    hasher: Sha256,
}

impl<W: AsyncWrite + Unpin> AsyncHashingWriteStream<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        self.hasher.finalize().to_vec()
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for AsyncHashingWriteStream<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                self.hasher.update(&buf[..*n]);
            }
        }
        result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn expected_hash(data: &[u8]) -> Vec<u8> {
        Sha256::digest(data).to_vec()
    }

    #[test]
    fn hashing_reader_hashes_only_consumed_bytes() {
        let mut reader = HashingReadStream::new(&b"abcdef"[..]);
        let mut bytes = [0; 3];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(reader.finalize(), expected_hash(b"abc"));
    }

    #[test]
    fn hashing_writer_hashes_only_written_bytes() {
        let mut writer = HashingWriteStream::new(Vec::new());
        writer.write_all(b"abc").unwrap();
        writer.write_all(b"def").unwrap();
        assert_eq!(writer.finalize(), expected_hash(b"abcdef"));
    }

    #[tokio::test]
    async fn async_hashing_reader_hashes_every_read() {
        let mut reader = AsyncHashingReadStream::new(&b"abcdef"[..]);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"abcdef");
        assert_eq!(reader.finalize(), expected_hash(b"abcdef"));
    }

    #[tokio::test]
    async fn async_hashing_writer_hashes_every_write() {
        let mut writer = AsyncHashingWriteStream::new(Vec::new());
        writer.write_all(b"abc").await.unwrap();
        writer.write_all(b"def").await.unwrap();
        writer.flush().await.unwrap();
        assert_eq!(writer.finalize(), expected_hash(b"abcdef"));
    }

    #[test]
    fn empty_streams_produce_the_sha256_empty_digest() {
        assert_eq!(
            HashingReadStream::new(&b""[..]).finalize(),
            expected_hash(b"")
        );
        assert_eq!(
            HashingWriteStream::new(Vec::<u8>::new()).finalize(),
            expected_hash(b"")
        );
    }
}
