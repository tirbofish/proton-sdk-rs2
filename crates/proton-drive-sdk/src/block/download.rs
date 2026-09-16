use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::pgp::PgpSessionKey;
use sha2::Digest;
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Clone)]
pub struct BlockDownloader {
    pub queue: crate::node::transfer::TransferQueue,
}

impl BlockDownloader {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            queue: crate::node::transfer::TransferQueue::new(max_degree_of_parallelism),
        }
    }

    pub async fn download(
        &self,
        client: &ProtonDriveClient,
        revision_uid: RevisionUid,
        index: i32,
        bare_url: String,
        token: String,
        content_key: PgpSessionKey,
        output_stream: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<Vec<u8>> {
        let _permit = self.queue.start_block().await?;
        let mut current_url = bare_url;
        let mut current_token = token;
        let mut refreshed_target = false;
        let mut last_error = None;
        let mut was_throttled = false;

        for attempt in 0..=4u32 {
            if attempt > 0 {
                let delay = last_error
                    .as_ref()
                    .and_then(|error: &anyhow::Error| {
                        error
                            .downcast_ref::<crate::error::TooManyRequestsException>()
                            .and_then(|error| error.retry_after)
                            .or_else(|| {
                                error
                                    .downcast_ref::<crate::error::HttpTransferError>()
                                    .and_then(|error| error.retry_after)
                            })
                    })
                    .unwrap_or_else(|| crate::error::retry_backoff_delay(attempt));
                tokio::time::sleep(delay).await;
            }

            let result = async {
                let response = client
                    .api()
                    .storage()
                    .get_blob_stream(&current_url, &current_token)
                    .await?;
                let bytes = response.bytes().await?;
                let mut hasher = sha2::Sha256::new();
                hasher.update(&bytes);
                let digest = hasher.finalize().to_vec();
                let plaintext = content_key.decrypt(&bytes)?;
                Ok::<_, anyhow::Error>((plaintext, digest))
            }
            .await;

            match result {
                Ok((plaintext, digest)) => {
                    output_stream.write_all(&plaintext).await?;
                    if was_throttled {
                        client.sdk_events().requests_unthrottled();
                    }
                    return Ok(digest);
                }
                Err(error) => {
                    if let Some(http_error) =
                        error.downcast_ref::<crate::error::HttpTransferError>()
                    {
                        if http_error.is_expired_target() && !refreshed_target {
                            let Some((url, new_token)) =
                                refresh_target(client, &revision_uid, index).await?
                            else {
                                return Err(error);
                            };
                            current_url = url;
                            current_token = new_token;
                            refreshed_target = true;
                            last_error = Some(error);
                            continue;
                        }
                        if !http_error.is_retryable() {
                            return Err(error);
                        }
                    } else if let Some(_throttled) =
                        error.downcast_ref::<crate::error::TooManyRequestsException>()
                    {
                        client.sdk_events().requests_throttled();
                        was_throttled = true;
                    } else if !error
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(|error| error.is_connect() || error.is_timeout())
                    {
                        // Decryption and local I/O failures are not fixed by
                        // retrying the same blob target.
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("block download failed")))
    }
}

async fn refresh_target(
    client: &ProtonDriveClient,
    revision_uid: &RevisionUid,
    index: i32,
) -> anyhow::Result<Option<(String, String)>> {
    let response = client
        .api()
        .files()
        .get_revision(
            revision_uid.node_uid.volume_id.clone(),
            revision_uid.node_uid.link_id.clone(),
            revision_uid.revision_id.clone(),
            Some(index),
            Some(1),
            false,
        )
        .await?;

    Ok(response
        .revision
        .blocks
        .into_iter()
        .find(|block| block.index == index)
        .map(|block| (block.bare_url, block.token)))
}
