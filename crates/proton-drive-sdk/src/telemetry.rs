//! Integrity metric context matching the upstream SDK telemetry schema.

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Months, Utc};
use parking_lot::Mutex;
use serde::Serialize;

use crate::client::ProtonDriveClient;
use crate::node::NodeUid;
use crate::node::crypto::{DecryptionOutput, LinkDecryptionResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricItemRecency {
    PastMonth,
    PastYear,
    #[serde(rename = "since_2024")]
    Since2024,
    #[serde(rename = "before_2024")]
    Before2024,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MetricItemCreator {
    #[serde(rename = "1p")]
    FirstParty,
    #[serde(rename = "3p-sdk")]
    ThirdPartySdk,
    #[serde(rename = "3p")]
    ThirdParty,
}

/// Calendar months are clamped to their last day, matching upstream's UTC logic.
pub fn get_metric_recency(creation_time: DateTime<Utc>, now: DateTime<Utc>) -> MetricItemRecency {
    if now
        .checked_sub_months(Months::new(1))
        .is_some_and(|past_month| creation_time >= past_month)
    {
        return MetricItemRecency::PastMonth;
    }
    // JavaScript's setUTCFullYear rolls February 29 forward to March 1.
    let past_year = now
        .with_year(now.year() - 1)
        .or_else(|| now.with_day(1)?.with_month(3)?.with_year(now.year() - 1));
    if past_year.is_some_and(|past_year| creation_time >= past_year) {
        return MetricItemRecency::PastYear;
    }
    if creation_time.timestamp() >= 1_704_067_200 {
        MetricItemRecency::Since2024
    } else {
        MetricItemRecency::Before2024
    }
}

pub fn get_metric_item_creator(
    third_party: Option<bool>,
    sdk: Option<bool>,
) -> Option<MetricItemCreator> {
    third_party.map(|third_party| {
        if !third_party {
            MetricItemCreator::FirstParty
        } else if sdk == Some(true) {
            MetricItemCreator::ThirdPartySdk
        } else {
            MetricItemCreator::ThirdParty
        }
    })
}

#[derive(Debug, Clone)]
pub(crate) struct MetricItem {
    pub uid: String,
    pub creation_time: Option<DateTime<Utc>>,
    pub third_party: Option<bool>,
    pub sdk: Option<bool>,
}

impl MetricItem {
    pub fn from_revision(uid: &NodeUid, revision: &crate::api::revision::RevisionDto) -> Self {
        Self {
            uid: uid.to_string(),
            creation_time: Some(revision.creation_time),
            third_party: revision.third_party,
            sdk: revision.sdk,
        }
    }

    pub fn from_link(
        volume_id: crate::volume::VolumeId,
        link: &crate::api::links::LinkDto,
    ) -> Self {
        Self {
            uid: NodeUid::new(volume_id, link.id.clone()).to_string(),
            creation_time: Some(link.creation_time),
            third_party: link.third_party,
            sdk: link.sdk,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IntegrityMetric<'a> {
    event_name: &'a str,
    field: &'a str,
    uid: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recency: Option<MetricItemRecency>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<MetricItemCreator>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address_matching_default_share: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Default)]
pub(crate) struct IntegrityReporter {
    decryption_errors: Mutex<HashSet<String>>,
    verification_errors: Mutex<HashSet<String>>,
}

impl IntegrityReporter {
    pub async fn report(
        &self,
        client: &ProtonDriveClient,
        item: &MetricItem,
        field: &str,
        error: Option<&str>,
        verification: bool,
        claimed_author: Option<&str>,
    ) {
        let reported = if verification {
            &self.verification_errors
        } else {
            &self.decryption_errors
        };
        if !reported.lock().insert(item.uid.clone()) {
            return;
        }
        self.report_unchecked(client, item, field, error, verification, claimed_author)
            .await;
    }

    pub async fn report_unchecked(
        &self,
        client: &ProtonDriveClient,
        item: &MetricItem,
        field: &str,
        error: Option<&str>,
        verification: bool,
        claimed_author: Option<&str>,
    ) {
        // Failed author lookup must never replace the underlying crypto result.
        let address_matching_default_share = if verification && claimed_author.is_some() {
            match (claimed_author, client.my_files_member_address_id().await) {
                (Some(author), Ok(id)) => client
                    .account()
                    .get_address(&id)
                    .await
                    .ok()
                    .map(|address| address.email_address == author),
                _ => None,
            }
        } else {
            None
        };
        let event_name = if verification {
            "verificationError"
        } else {
            "decryptionError"
        };
        let metric = IntegrityMetric {
            event_name,
            field,
            uid: &item.uid,
            recency: item
                .creation_time
                .map(|time| get_metric_recency(time, Utc::now())),
            created_by: get_metric_item_creator(item.third_party, item.sdk),
            address_matching_default_share,
            error,
        };
        if let Ok(payload) = serde_json::to_vec(&metric) {
            client
                .telemetry()
                .record_metric(event_name.into(), Some(payload))
                .await;
        }
    }

    pub async fn decrypt_message<'a>(
        &self,
        client: &ProtonDriveClient,
        item: &MetricItem,
        field: &str,
        message: &crate::pgp::PgpArmoredMessage,
        signature: Option<&crate::pgp::PgpArmoredSignature>,
        keys: impl IntoIterator<Item = &'a crate::pgp::PgpPrivateKey>,
        claim: &crate::node::authorship::AuthorshipClaim,
    ) -> Result<
        (
            Vec<u8>,
            Option<proton_rpgp::SessionKey>,
            Option<crate::node::crypto::AuthorshipVerificationFailure>,
        ),
        String,
    > {
        let result =
            crate::node::crypto::NodeCrypto::decrypt_message(message, signature, keys, claim);
        match &result {
            Err(error) => {
                self.report(client, item, field, Some(error), false, None)
                    .await
            }
            Ok((_, _, Some(_))) => {
                self.report(
                    client,
                    item,
                    field,
                    None,
                    true,
                    claim.author.email_address.as_deref(),
                )
                .await
            }
            _ => {}
        }
        result
    }

    pub async fn report_link(
        &self,
        client: &ProtonDriveClient,
        item: &MetricItem,
        link: &LinkDecryptionResult,
        had_parent_key: bool,
        name_author: Option<&str>,
    ) {
        if had_parent_key {
            if let Err(error) = &link.node_key {
                self.report(client, item, "nodeKey", Some(error), false, None)
                    .await;
            }
        }
        if let Ok(passphrase) = &link.passphrase {
            if passphrase.authorship_verification_failure.is_some() {
                self.report(
                    client,
                    item,
                    "nodeKey",
                    None,
                    true,
                    link.node_authorship_claim.author.email_address.as_deref(),
                )
                .await;
            }
        }
        self.report_output(client, item, "nodeName", &link.name, name_author)
            .await;
    }

    pub async fn report_active_revision_attributes(
        &self,
        client: &ProtonDriveClient,
        uid: &NodeUid,
        revision: &crate::api::revision::ActiveRevisionDto,
        key: &crate::pgp::PgpPrivateKey,
        claim: &crate::node::authorship::AuthorshipClaim,
    ) {
        let Some(message) = &revision.extended_attributes else {
            return;
        };
        // Revision creation time and provenance can differ from the containing node.
        let item = MetricItem {
            uid: uid.to_string(),
            creation_time: Some(revision.creation_time),
            third_party: revision.third_party,
            sdk: revision.sdk,
        };
        if let Ok((bytes, _, _)) = self
            .decrypt_message(
                client,
                &item,
                "nodeExtendedAttributes",
                message,
                None,
                [key],
                claim,
            )
            .await
        {
            if let Err(error) =
                serde_json::from_slice::<crate::api::attr::ExtendedAttributes>(&bytes)
            {
                self.report(
                    client,
                    &item,
                    "nodeExtendedAttributes",
                    Some(&error.to_string()),
                    false,
                    None,
                )
                .await;
            }
        }
    }

    pub async fn report_output<T>(
        &self,
        client: &ProtonDriveClient,
        item: &MetricItem,
        field: &str,
        result: &Result<DecryptionOutput<T>, Option<String>>,
        author: Option<&str>,
    ) {
        match result {
            Err(Some(error)) => {
                self.report(client, item, field, Some(error), false, None)
                    .await
            }
            Ok(output) if output.authorship_verification_failure.is_some() => {
                self.report(client, item, field, None, true, author).await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    #[tokio::test]
    async fn integrity_metrics_deduplicate_by_node_and_keep_error_kinds_separate() {
        let (client, telemetry) = crate::test_support::client().await;
        let clone = client.clone();
        let item = MetricItem {
            uid: "volume~node".into(),
            creation_time: Some(time("2023-12-31T23:59:59Z")),
            third_party: Some(true),
            sdk: Some(true),
        };
        client
            .integrity_reporter()
            .report(&client, &item, "nodeKey", Some("invalid key"), false, None)
            .await;
        clone
            .integrity_reporter()
            .report(&clone, &item, "nodeName", Some("invalid name"), false, None)
            .await;
        client
            .integrity_reporter()
            .report(
                &client,
                &item,
                "nodeKey",
                None,
                true,
                Some("member@example.test"),
            )
            .await;
        let metrics = telemetry.0.lock();
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].0, "decryptionError");
        assert_eq!(
            metrics[0].1,
            serde_json::json!({
                "eventName": "decryptionError", "field": "nodeKey", "uid": "volume~node",
                "recency": "before_2024", "createdBy": "3p-sdk", "error": "invalid key"
            })
        );
        assert_eq!(metrics[1].0, "verificationError");
        assert_eq!(metrics[1].1["addressMatchingDefaultShare"], true);
        assert!(metrics[1].1.get("volumeType").is_none());
        assert!(metrics[1].1.get("fromBefore2024").is_none());
    }

    #[tokio::test]
    async fn node_conversion_reports_crypto_failure_without_changing_the_error() {
        let (client, telemetry) = crate::test_support::client().await;
        let details: crate::api::links::LinkDetailsDto =
            serde_json::from_value(serde_json::json!({
                "Link": {
                    "LinkID": "broken-folder", "Type": 1, "State": 1,
                    "CreateTime": 1_600_000_000, "ModifyTime": 1_600_000_000,
                    "Name": "invalid", "NameHash": "", "NodeKey": "invalid",
                    "NodePassphrase": "invalid", "ThirdParty": false, "Sdk": true
                },
                "Folder": { "NodeHashKey": "invalid" }
            }))
            .unwrap();
        let parent = crate::crypto::CryptoGenerator::generate_private_key().unwrap();
        let error = crate::node::DtoToMetadataConverter::convert_dto_to_node_metadata_with_client(
            &client,
            crate::volume::VolumeId::new("volume".into()),
            details,
            Some(&parent),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Decryption failed for folder");
        let metrics = telemetry.0.lock();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].1["field"], "nodeKey");
        assert_eq!(metrics[0].1["createdBy"], "1p");
        assert_eq!(metrics[0].1["uid"], "volume~broken-folder");
    }

    #[tokio::test]
    async fn revision_metrics_preserve_revision_origin_and_optional_context() {
        let (client, telemetry) = crate::test_support::client().await;
        let revision: crate::api::revision::RevisionDto =
            serde_json::from_value(serde_json::json!({
                "ID": "revision", "CreateTime": 1_600_000_000, "Size": 1, "State": 1,
                "ThirdParty": true, "Sdk": false
            }))
            .unwrap();
        let item = MetricItem::from_revision(&NodeUid::from_parts("volume", "node"), &revision);
        client
            .integrity_reporter()
            .report(
                &client,
                &item,
                "nodeExtendedAttributes",
                Some("invalid attributes"),
                false,
                None,
            )
            .await;
        let unknown = MetricItem {
            uid: "share".into(),
            creation_time: None,
            third_party: None,
            sdk: None,
        };
        client
            .integrity_reporter()
            .report(&client, &unknown, "shareKey", Some("invalid"), false, None)
            .await;
        let metrics = telemetry.0.lock();
        assert_eq!(metrics[0].1["createdBy"], "3p");
        assert_eq!(metrics[0].1["recency"], "before_2024");
        assert!(metrics[1].1.get("createdBy").is_none());
        assert!(metrics[1].1.get("recency").is_none());
    }

    #[tokio::test]
    async fn active_revision_attribute_errors_use_revision_context_and_skip_missing_attributes() {
        let (client, telemetry) = crate::test_support::client().await;
        let mut revision: crate::api::revision::ActiveRevisionDto =
            serde_json::from_value(serde_json::json!({
                "RevisionID": "revision", "CreateTime": 1_600_000_000, "EncryptedSize": 1,
                "Thumbnails": [], "ThirdParty": true, "Sdk": true
            }))
            .unwrap();
        let key = crate::crypto::CryptoGenerator::generate_private_key().unwrap();
        let claim = crate::node::authorship::AuthorshipClaim {
            keys: Vec::new(),
            author: crate::author::Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        let uid = NodeUid::from_parts("volume", "node");
        client
            .integrity_reporter()
            .report_active_revision_attributes(&client, &uid, &revision, &key, &claim)
            .await;
        assert!(telemetry.0.lock().is_empty());
        revision.extended_attributes = Some(crate::pgp::PgpArmoredMessage("invalid".into()));
        client
            .integrity_reporter()
            .report_active_revision_attributes(&client, &uid, &revision, &key, &claim)
            .await;
        let metrics = telemetry.0.lock();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].1["field"], "nodeExtendedAttributes");
        assert_eq!(metrics[0].1["createdBy"], "3p-sdk");
        assert_eq!(metrics[0].1["recency"], "before_2024");
    }

    #[test]
    fn recency_matches_upstream_calendar_boundaries() {
        let now = time("2026-09-22T12:00:00Z");
        for (creation, expected) in [
            ("2026-09-01T12:00:00Z", MetricItemRecency::PastMonth),
            ("2026-08-22T12:00:00Z", MetricItemRecency::PastMonth),
            ("2026-08-22T11:59:59Z", MetricItemRecency::PastYear),
            ("2025-09-22T12:00:00Z", MetricItemRecency::PastYear),
            ("2025-09-22T11:59:59Z", MetricItemRecency::Since2024),
            ("2024-01-01T00:00:00Z", MetricItemRecency::Since2024),
            ("2023-12-31T23:59:59Z", MetricItemRecency::Before2024),
        ] {
            assert_eq!(get_metric_recency(time(creation), now), expected);
        }
        assert_eq!(
            get_metric_recency(time("2026-02-28T12:00:00Z"), time("2026-03-31T12:00:00Z")),
            MetricItemRecency::PastMonth
        );
        assert_eq!(
            get_metric_recency(time("2026-02-28T11:59:59Z"), time("2026-03-31T12:00:00Z")),
            MetricItemRecency::PastYear
        );
        assert_eq!(
            get_metric_recency(time("2023-02-28T12:00:00Z"), time("2024-02-29T12:00:00Z")),
            MetricItemRecency::Before2024
        );
        assert_eq!(
            get_metric_recency(time("2023-03-01T12:00:00Z"), time("2024-02-29T12:00:00Z")),
            MetricItemRecency::PastYear
        );
    }

    #[test]
    fn creator_preserves_unknown_provenance() {
        assert_eq!(get_metric_item_creator(None, Some(true)), None);
        for (third_party, sdk, expected) in [
            (false, false, MetricItemCreator::FirstParty),
            (false, true, MetricItemCreator::FirstParty),
            (true, true, MetricItemCreator::ThirdPartySdk),
            (true, false, MetricItemCreator::ThirdParty),
        ] {
            assert_eq!(
                get_metric_item_creator(Some(third_party), Some(sdk)),
                Some(expected)
            );
        }
        assert_eq!(
            get_metric_item_creator(Some(true), None),
            Some(MetricItemCreator::ThirdParty)
        );
    }
}
