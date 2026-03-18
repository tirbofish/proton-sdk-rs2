use crate::client::ProtonDriveClient;
use crate::share::{Share, ShareId};
use crate::pgp::{PgpPrivateKey, PgpArmoredPrivateKey, PgpArmoredMessage, PgpArmoredSignature};
use crate::node::secrets::ShareAndKey;
use proton_rpgp::AccessKeyInfo;

pub struct ShareOperations;

impl ShareOperations {
    pub async fn get_share(
        client: &ProtonDriveClient,
        share_id: ShareId,
    ) -> anyhow::Result<ShareAndKey> {
        if let Some(share) = client.cache().entities().try_get_share(share_id.clone()).await? {
            if let Some(key) = client.cache().secrets().try_get_share_key(share_id.clone()).await? {
                return Ok(ShareAndKey { share, key });
            }
        }

        let response = client.api().shares().get_share(share_id.clone()).await?;
        
        // Match C# logic: decrypt share key
        let (share, key) = ShareCrypto::decrypt_share(
            client,
            response.id.clone(),
            &response.key,
            &response.passphrase,
            &response.passphrase_signature,
            response.invitee_share_passphrase_session_key_signature.as_ref(),
            &response.creator_email_address,
            &response.address_id,
        ).await?;

        client.cache().entities().set_share(share.clone()).await?;
        client.cache().secrets().set_share_key(share.id.clone(), key.clone()).await?;

        Ok(ShareAndKey { share, key })
    }
}

pub struct ShareCrypto;

impl ShareCrypto {
    pub async fn decrypt_share(
        client: &ProtonDriveClient,
        share_id: ShareId,
        encrypted_key: &PgpArmoredPrivateKey,
        encrypted_passphrase: &PgpArmoredMessage,
        passphrase_signature: &PgpArmoredSignature,
        invitee_signature: Option<&PgpArmoredSignature>,
        creator_email: &str,
        address_id: &crate::account::AddressId,
    ) -> anyhow::Result<(Share, PgpPrivateKey)> {
        let address_keys = client.account().get_address_private_keys(&address_id).await?;
        let user_keys = client.account().get_user_keys().await?;

        let mut all_keys = Vec::new();
        for k in address_keys {
            all_keys.push(PgpPrivateKey(k));
        }
        for k in user_keys {
            all_keys.push(PgpPrivateKey(k));
        }

        let authorship_claim = crate::node::authorship::AuthorshipClaim::create(
            client.account().clone(),
            Some(creator_email),
        ).await;

        match crate::node::crypto::NodeCrypto::decrypt_message(
            encrypted_passphrase,
            invitee_signature.or(Some(passphrase_signature)),
            &all_keys,
            &authorship_claim,
        ) {
            Ok((passphrase, _, _)) => {
                // Unlock the share private key using the decrypted passphrase
                let share_key = crate::node::crypto::NodeCrypto::unlock_key_with_passphrase(
                    encrypted_key,
                    &passphrase,
                ).map_err(|e| anyhow::anyhow!("Failed to unlock share key: {:?}", e))?;

                let response = client.api().shares().get_share(share_id).await?;


                let share = Share {
                    id: response.id,
                    root_folder_id: crate::node::NodeUid::new(response.volume_id, response.root_link_id),
                    membership_address_id: response.address_id,
                    share_type: response.r#type,
                };

                return Ok((share, share_key));
            }
            Err(e) => {
                anyhow::bail!("Failed to decrypt share passphrase: {}", e)
            }
        }
    }
}
