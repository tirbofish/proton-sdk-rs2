//! Diagnostic helpers ported from the TypeScript SDK.

use futures::Stream;
use futures::StreamExt;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ZipOptions {
    pub stop_on_first_done: bool,
}

impl Default for ZipOptions {
    fn default() -> Self {
        Self {
            stop_on_first_done: false,
        }
    }
}

pub async fn zip_generators<A, B, T>(left: A, right: B, options: ZipOptions) -> Vec<T>
where
    A: Stream<Item = T> + Unpin,
    B: Stream<Item = T> + Unpin,
{
    let mut left = left.fuse();
    let mut right = right.fuse();
    let mut out = Vec::new();
    let mut left_done = false;
    let mut right_done = false;

    loop {
        tokio::select! {
            item = left.next(), if !left_done => {
                match item {
                    Some(value) => out.push(value),
                    None => {
                        left_done = true;
                        if options.stop_on_first_done {
                            break;
                        }
                    }
                }
            }
            item = right.next(), if !right_done => {
                match item {
                    Some(value) => out.push(value),
                    None => {
                        right_done = true;
                        if options.stop_on_first_done {
                            break;
                        }
                    }
                }
            }
            else => break,
        }
    }
    out
}

pub async fn async_iterator_map<S, F, Fut, T, U>(
    input: S,
    mapper: F,
    concurrency: usize,
) -> anyhow::Result<Vec<U>>
where
    S: Stream<Item = anyhow::Result<T>> + Unpin,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<U>>,
{
    let mapped = input.map(|item| async { mapper(item?).await });
    let mut mapped = mapped.buffer_unordered(concurrency.max(1));
    let mut out = Vec::new();
    while let Some(item) = mapped.next().await {
        out.push(item?);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct DiagnosticArchiveFile {
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticResult {
    pub kind: String,
    pub message: String,
}

pub fn generate_diagnostic_zip(files: &[DiagnosticArchiveFile]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut entries = 0u16;

    for file in files {
        let name = file.name.as_bytes();
        anyhow::ensure!(
            name.len() <= u16::MAX as usize,
            "diagnostic zip entry name is too long"
        );
        let crc = crc32(&file.contents);
        let size = file.contents.len() as u32;
        let local_offset = out.len() as u32;

        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&file.contents);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&local_offset.to_le_bytes());
        central.extend_from_slice(name);
        entries += 1;
    }

    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

pub fn generate_diagnostic_zip_from_results(
    results: &[DiagnosticResult],
) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec_pretty(results)?;
    let summary = results
        .iter()
        .map(|result| format!("{}: {}", result.kind, result.message))
        .collect::<Vec<_>>()
        .join("\n");
    generate_diagnostic_zip(&[
        DiagnosticArchiveFile {
            name: "diagnostic/results.json".into(),
            contents: json,
        },
        DiagnosticArchiveFile {
            name: "diagnostic/summary.txt".into(),
            contents: summary.into_bytes(),
        },
    ])
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn zip_generators_handles_empty_inputs() {
        let left = stream::empty::<u8>();
        let right = stream::empty::<u8>();
        assert!(
            zip_generators(left, right, ZipOptions::default())
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn zip_generators_handles_one_empty_side() {
        let left = stream::empty::<i32>();
        let right = stream::iter([1, 2]);
        assert_eq!(
            zip_generators(left, right, ZipOptions::default()).await,
            vec![1, 2]
        );

        let left = stream::iter(["a", "b"]);
        let right = stream::empty();
        assert_eq!(
            zip_generators(left, right, ZipOptions::default()).await,
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn zip_generators_merges_both_streams() {
        let left = stream::iter(["a1", "a2", "a3"]);
        let right = stream::iter(["b1", "b2", "b3"]);
        let mut result = zip_generators(left, right, ZipOptions::default()).await;
        result.sort();
        assert_eq!(result, vec!["a1", "a2", "a3", "b1", "b2", "b3"]);
    }

    #[tokio::test]
    async fn zip_generators_can_stop_when_the_first_stream_ends() {
        let left = stream::iter([1, 2]);
        let right = stream::iter(std::iter::repeat(99).take(100));
        let result = zip_generators(
            left,
            right,
            ZipOptions {
                stop_on_first_done: true,
            },
        )
        .await;
        assert!(result.contains(&1));
        assert!(result.contains(&2));
        assert!(result.len() < 102);
    }

    #[tokio::test]
    async fn async_iterator_map_transforms_values() {
        let input = stream::iter([1, 2, 3, 4, 5]).map(Ok);
        let out = async_iterator_map(input, |x| async move { Ok(x * 2) }, 2)
            .await
            .unwrap();
        let mut sorted = out;
        sorted.sort();
        assert_eq!(sorted, vec![2, 4, 6, 8, 10]);
    }

    #[tokio::test]
    async fn async_iterator_map_handles_empty_input() {
        let input = stream::iter(Vec::<anyhow::Result<i32>>::new());
        let out = async_iterator_map(input, |x| async move { Ok(x * 2) }, 2)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn diagnostic_zip_contains_stored_entries() {
        let zip = generate_diagnostic_zip_from_results(&[DiagnosticResult {
            kind: "node".into(),
            message: "ok".into(),
        }])
        .unwrap();
        assert!(zip.windows(4).any(|window| window == b"PK\x03\x04"));
        let pretty = br#""kind": "node""#;
        let compact = br#""kind":"node""#;
        assert!(
            zip.windows(pretty.len()).any(|window| window == pretty)
                || zip.windows(compact.len()).any(|window| window == compact)
        );
        assert!(
            zip.windows(b"node: ok".len())
                .any(|window| window == b"node: ok")
        );
        assert_eq!(crc32(b"123"), 0x8848_63d2);
    }
}
