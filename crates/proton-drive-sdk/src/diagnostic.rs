//! Diagnostic helpers ported from the TypeScript SDK.
//!
//! `zip_generators` is implemented. ZIP archive generation remains a stub.

use futures::Stream;
use futures::StreamExt;

pub async fn zip_generators<A, B, T>(mut left: A, mut right: B) -> Vec<T>
where
    A: Stream<Item = T> + Unpin,
    B: Stream<Item = T> + Unpin,
{
    let mut out = Vec::new();
    loop {
        tokio::select! {
            biased;
            left_item = left.next() => {
                match left_item {
                    Some(item) => out.push(item),
                    None => {
                        while let Some(item) = right.next().await {
                            out.push(item);
                        }
                        break;
                    }
                }
            }
            right_item = right.next() => {
                match right_item {
                    Some(item) => out.push(item),
                    None => {
                        while let Some(item) = left.next().await {
                            out.push(item);
                        }
                        break;
                    }
                }
            }
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

pub fn generate_diagnostic_zip_stub() -> Result<Vec<u8>, crate::error::ProtonDriveError> {
    Err(crate::error::ProtonDriveError::Unimplemented(
        "diagnostic zip generation is not implemented".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn zip_generators_handles_empty_inputs() {
        let left = stream::empty::<u8>();
        let right = stream::empty::<u8>();
        assert!(zip_generators(left, right).await.is_empty());
    }

    #[tokio::test]
    async fn zip_generators_handles_one_empty_side() {
        let left = stream::empty::<i32>();
        let right = stream::iter([1, 2]);
        assert_eq!(zip_generators(left, right).await, vec![1, 2]);

        let left = stream::iter(["a", "b"]);
        let right = stream::empty();
        assert_eq!(zip_generators(left, right).await, vec!["a", "b"]);
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
    fn diagnostic_zip_is_stubbed() {
        let error = generate_diagnostic_zip_stub().unwrap_err();
        assert!(matches!(
            error,
            crate::error::ProtonDriveError::Unimplemented(_)
        ));
    }
}
