use std::sync::Arc;

use tokio::sync::{OnceCell, RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::{SessionId, api::response::{AuthenticationResponse, SesisonInitiationResponse, RefreshSessionResponse}};

pub struct TokenCredential {
    client: Arc<dyn AuthenticationApiClientTrait>,
    session_id: SessionId,
    access_token: String,
    refresh_token: String,
    tokens_task: Arc<RwLock<Arc<OnceCell<(String, String)>>>>,

    tokens_refreshed_tx: broadcast::Sender<(String, String)>,
    refresh_token_expired_tx: broadcast::Sender<()>,
}

impl TokenCredential {
    pub fn new(
        client: Arc<dyn AuthenticationApiClientTrait>,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
    ) -> Self {
        let tokens_task = OnceCell::new();
        let _ = tokens_task.set((access_token.clone(), refresh_token.clone()));

        let (tokens_refreshed_tx, _) = broadcast::channel(16);
        let (refresh_token_expired_tx, _) = broadcast::channel(16);
        
        Self {
            client,
            session_id,
            tokens_task: Arc::new(RwLock::new(Arc::new(tokens_task))),
            tokens_refreshed_tx,
            refresh_token_expired_tx,
            access_token,
            refresh_token,
        }
    }

    pub async fn get_tokens(
        &self,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<(String, String)> {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                Err(anyhow::anyhow!("Operation cancelled"))
            }
            result = async {
                let task = self.tokens_task.read().await.clone();
                let tokens = task.get().ok_or_else(|| anyhow::anyhow!("Tokens not initialized"))?;
                Ok((tokens.0.clone(), tokens.1.clone()))
            } => result
        }
    }

    pub async fn get_access_token(
        &self,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<(String, String)> {
        self.get_tokens(cancellation_token).await
    }

    pub fn subscribe_tokens_refreshed(&self) -> broadcast::Receiver<(String, String)> {
        self.tokens_refreshed_tx.subscribe()
    }
    
    pub fn subscribe_refresh_token_expired(&self) -> broadcast::Receiver<()> {
        self.refresh_token_expired_tx.subscribe()
    }
    
    /// aka TokenCredential.OnTokensRefreshed
    fn trigger_tokens_refreshed(&self, access_token: String, refresh_token: String) {
        let _ = self.tokens_refreshed_tx.send((access_token, refresh_token));
    }
    
    /// aka TokenCredential.OnRefreshTokenExpired
    fn trigger_refresh_token_expired(&self) {
        let _ = self.refresh_token_expired_tx.send(());
    }

    pub async fn get_refreshed_access_token(
        &self,
        rejected_access_token: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<String> {
        let current_tokens_task = self.tokens_task.read().await.clone();

        let (current_access_token, current_refresh_token) = tokio::select! {
            _ = cancellation_token.cancelled() => {
                return Err(anyhow::anyhow!("Operation cancelled"));
            }
            result = async {
                let tokens = current_tokens_task.get().ok_or_else(|| anyhow::anyhow!("Tokens not initialized"))?;
                Ok::<_, anyhow::Error>((tokens.0.clone(), tokens.1.clone()))
            } => result?
        };

        let is_likely_already_refreshed = current_access_token != rejected_access_token;
        if is_likely_already_refreshed {
            return Ok(current_access_token);
        }

        let refreshed_tokens_task = Arc::new(OnceCell::new());
        let refreshed_task_clone = refreshed_tokens_task.clone();
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let current_access = current_access_token.clone();
        let current_refresh = current_refresh_token.clone();
        let cancellation = cancellation_token.clone();

        let refresh_handle = tokio::spawn(async move {
            let result = async {
                let response = client
                    .refresh_session(
                        session_id,
                        current_access.clone(),
                        current_refresh.clone(),
                        cancellation,
                    )
                    .await?;
                Ok::<_, anyhow::Error>((response.access_token, response.refresh_token))
            }
            .await;

            match result {
                Ok(tokens) => {
                    let _ = refreshed_task_clone.set(tokens);
                }
                Err(_) => {
                    let _ = refreshed_task_clone.set((current_access, current_refresh));
                }
            }
        });

        let mut tokens_task_guard = self.tokens_task.write().await;
        let tokens_task_replaced = Arc::ptr_eq(&*tokens_task_guard, &current_tokens_task);
        
        if tokens_task_replaced {
            *tokens_task_guard = refreshed_tokens_task.clone();
        }
        drop(tokens_task_guard);

        tokio::select! {
            _ = cancellation_token.cancelled() => {
                return Err(anyhow::anyhow!("Operation cancelled"));
            }
            result = refresh_handle => {
                result.map_err(|e| anyhow::anyhow!("Refresh task panicked: {}", e))?;
            }
        }

        let (access_token, refresh_token) = refreshed_tokens_task
            .get()
            .ok_or_else(|| anyhow::anyhow!("Failed to get refreshed tokens"))?
            .clone();

        if tokens_task_replaced {
            self.trigger_tokens_refreshed(access_token.clone(), refresh_token);
        }

        Ok(access_token)
    }
}


#[async_trait::async_trait]
pub trait AuthenticationApiClientTrait: Send + Sync {
    async fn initiate_session(
        &self,
        username: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<SesisonInitiationResponse>;

    async fn authenticate(
        &self,
        initiation_response: SesisonInitiationResponse,
        srp_client_handshake: proton_crypto::srp::ClientProof,
    ) -> anyhow::Result<AuthenticationResponse>;

    async fn refresh_session(
        &self,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<RefreshSessionResponse>;
}
