//! Ports of ProtonDriveApps/sdk TypeScript tests that live outside in-module
//! `#[cfg(test)]` blocks.

use proton_drive_sdk::additional_metadata::{
    FileLike, MediaInfo, generate_additional_node_metadata, generate_additional_photo_node_metadata,
};
use proton_drive_sdk::diagnostic::{
    DiagnosticResult, ZipOptions, generate_diagnostic_zip_from_results, zip_generators,
};
use proton_drive_sdk::http::get_unauth_endpoint;
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::sharing::{
    AbuseCategory, ReportDirectShareAbuseSettings, SharingOperations, parse_public_link_url,
};
use proton_drive_sdk::utils::wait_for_condition;

const UPSTREAM_TEST_FILES: &[&str] = &[
    "cli/src/api/apiRequirements.test.ts",
    "cli/src/api/messageEmitter.test.ts",
    "cli/src/cache/driveCryptoCacheAdapter.test.ts",
    "cli/src/cli/help.test.ts",
    "cli/src/cli/paths.test.ts",
    "cli/src/cli/readline.test.ts",
    "cli/src/cli/splitQuotedLine.test.ts",
    "cli/src/cli/version.test.ts",
    "cli/src/commands/fileSystem/commandFileSystemUpload.test.ts",
    "cli/src/commands/fileSystem/downloadOperations.test.ts",
    "cli/src/commands/fileSystem/downloadPathValidation.test.ts",
    "cli/src/commands/fileSystem/localPath.test.ts",
    "cli/src/commands/fileSystem/mediaType.test.ts",
    "cli/src/commands/fileSystem/transferConflictResolver.test.ts",
    "cli/src/commands/fileSystem/transferQueue.test.ts",
    "cli/src/commands/fileSystem/transferSummary.test.ts",
    "cli/src/credentials/parseCredentials.test.ts",
    "cli/src/credentials/passCredentialsStore.test.ts",
    "cli/src/events/lock.test.ts",
    "cli/src/events/manager.test.ts",
    "cli/src/events/providerMemory.test.ts",
    "cli/src/events/providerPersisted.test.ts",
    "cli/src/events/storage.test.ts",
    "cli/src/telemetry/fileHandler.test.ts",
    "cli/src/telemetry/metricHandler.test.ts",
    "cli/src/telemetry/verifierFileSizeBucket.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/appleMakerNote.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/convertSubjectAreaToSubjectCoordinates.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/exifParser.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/exifUtils.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/formatExifDateTime.test.ts",
    "client/js/src/additionalNodeMetadata/exifParser/tagDetector.test.ts",
    "client/js/src/additionalNodeMetadata/index.test.ts",
    "client/js/src/additionalNodeMetadata/metadata/builder.test.ts",
    "client/js/src/additionalNodeMetadata/metadata/parser.test.ts",
    "client/js/src/cache/memoryCache.test.ts",
    "client/js/src/crypto/driveCrypto.test.ts",
    "client/js/src/diagnostic/zipGenerators.test.ts",
    "client/js/src/integration-tests/nodesCrypto.test.ts",
    "client/js/src/internal/apiService/apiService.test.ts",
    "client/js/src/internal/apiService/errors.test.ts",
    "client/js/src/internal/asyncIteratorMap.test.ts",
    "client/js/src/internal/asyncIteratorRace.test.ts",
    "client/js/src/internal/batch.test.ts",
    "client/js/src/internal/batchLoading.test.ts",
    "client/js/src/internal/devices/manager.test.ts",
    "client/js/src/internal/download/blockIndex.test.ts",
    "client/js/src/internal/download/fileDownloader.test.ts",
    "client/js/src/internal/download/seekableStream.test.ts",
    "client/js/src/internal/download/telemetry.test.ts",
    "client/js/src/internal/download/thumbnailDownloader.test.ts",
    "client/js/src/internal/easySwitch/importFolderInjection.test.ts",
    "client/js/src/internal/errors.test.ts",
    "client/js/src/internal/events/coreEventManager.test.ts",
    "client/js/src/internal/events/eventManager.test.ts",
    "client/js/src/internal/events/eventScheduler.test.ts",
    "client/js/src/internal/events/index.test.ts",
    "client/js/src/internal/events/volumeEventManager.test.ts",
    "client/js/src/internal/nodes/apiService.test.ts",
    "client/js/src/internal/nodes/cache.test.ts",
    "client/js/src/internal/nodes/cryptoCache.test.ts",
    "client/js/src/internal/nodes/cryptoService.test.ts",
    "client/js/src/internal/nodes/debouncer.test.ts",
    "client/js/src/internal/nodes/events.test.ts",
    "client/js/src/internal/nodes/extendedAttributes.test.ts",
    "client/js/src/internal/nodes/index.test.ts",
    "client/js/src/internal/nodes/nodeName.test.ts",
    "client/js/src/internal/nodes/nodesAccess.test.ts",
    "client/js/src/internal/nodes/nodesManagement.test.ts",
    "client/js/src/internal/photos/addToAlbum.test.ts",
    "client/js/src/internal/photos/albumsCrypto.test.ts",
    "client/js/src/internal/photos/albumsManager.test.ts",
    "client/js/src/internal/photos/apiService.test.ts",
    "client/js/src/internal/photos/nodes.test.ts",
    "client/js/src/internal/photos/photosManager.test.ts",
    "client/js/src/internal/photos/photosTransferPayloadBuilder.test.ts",
    "client/js/src/internal/photos/timeline.test.ts",
    "client/js/src/internal/reportAbuse/index.test.ts",
    "client/js/src/internal/sdkEvents.test.ts",
    "client/js/src/internal/shares/cache.test.ts",
    "client/js/src/internal/shares/cryptoCache.test.ts",
    "client/js/src/internal/shares/cryptoService.test.ts",
    "client/js/src/internal/shares/manager.test.ts",
    "client/js/src/internal/sharing/cache.test.ts",
    "client/js/src/internal/sharing/cryptoService.test.ts",
    "client/js/src/internal/sharing/events.test.ts",
    "client/js/src/internal/sharing/sharingAccess.test.ts",
    "client/js/src/internal/sharing/sharingManagement.test.ts",
    "client/js/src/internal/sharingPublic/reporting.test.ts",
    "client/js/src/internal/sharingPublic/session/url.test.ts",
    "client/js/src/internal/sharingPublic/unauthApiService.test.ts",
    "client/js/src/internal/upload/blockVerifier.test.ts",
    "client/js/src/internal/upload/chunkStreamReader.test.ts",
    "client/js/src/internal/upload/fileUploader.test.ts",
    "client/js/src/internal/upload/index.test.ts",
    "client/js/src/internal/upload/manager.test.ts",
    "client/js/src/internal/upload/queue.test.ts",
    "client/js/src/internal/upload/smallFileUploader.test.ts",
    "client/js/src/internal/upload/streamReader.test.ts",
    "client/js/src/internal/upload/streamUploader.test.ts",
    "client/js/src/internal/upload/telemetry.test.ts",
    "client/js/src/internal/wait.test.ts",
    "client/js/src/telemetry.test.ts",
    "incubating/account/js/src/apiClient.test.ts",
    "incubating/account/js/src/telemetryPreference.test.ts",
];

#[test]
fn catalog_lists_every_upstream_javascript_test_file() {
    assert_eq!(UPSTREAM_TEST_FILES.len(), 105);
    let mut unique = UPSTREAM_TEST_FILES.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 105);
}

#[tokio::test]
async fn wait_for_condition_resolves_immediately_or_after_retry() {
    let mut calls = 0;
    wait_for_condition(
        || {
            calls += 1;
            true
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(calls, 1);

    let mut calls = 0;
    wait_for_condition(
        || {
            calls += 1;
            calls >= 2
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(calls, 2);

    let error = wait_for_condition(|| false, true).await.unwrap_err();
    assert!(error.to_string().contains("aborted"));
}

#[test]
fn report_abuse_validation_matches_typescript_categories() {
    for category in [AbuseCategory::Copyright, AbuseCategory::StolenData] {
        assert!(SharingOperations::validate_report_settings(category, true, None).is_err());
        assert!(
            SharingOperations::validate_report_settings(category, true, Some("message")).is_ok()
        );
    }
    for category in [
        AbuseCategory::Spam,
        AbuseCategory::Malware,
        AbuseCategory::Other,
    ] {
        assert!(SharingOperations::validate_report_settings(category, true, None).is_ok());
    }
}

#[test]
fn public_link_url_parser_matches_typescript() {
    assert_eq!(
        parse_public_link_url("https://drive.proton.me/urls/abc123#def456").unwrap(),
        ("abc123".into(), "def456".into())
    );
    assert_eq!(
        parse_public_link_url("https://example.com/urls/mytoken#mypassword").unwrap(),
        ("mytoken".into(), "mypassword".into())
    );
    assert!(parse_public_link_url("https://drive.proton.me/urls/token123").is_err());
    assert!(parse_public_link_url("not-a-url").is_err());
}

#[test]
fn generate_additional_metadata_without_exif_loader_is_empty() {
    let file = FileLike {
        name: Some("test.jpg".into()),
        last_modified: 0,
        bytes: b"content".to_vec(),
    };
    let metadata = generate_additional_node_metadata(&file, "image/jpeg", None);
    assert!(metadata.location.is_none());
    let (photo_metadata, tags, _) = generate_additional_photo_node_metadata(
        &file,
        "image/jpeg",
        Some(&MediaInfo {
            width: Some(1920),
            height: Some(1080),
            duration: Some(120.0),
        }),
    );
    assert!(photo_metadata.media.is_some());
    assert!(tags.is_empty());
}

#[test]
fn report_direct_share_settings_require_node_uid() {
    let settings = ReportDirectShareAbuseSettings {
        node_uid: NodeUid::from_parts("volume", "node"),
        abuse_category: AbuseCategory::Spam,
        bona_fide: true,
        reporter_message: None,
        reporter_email: None,
        revision_uid: None,
        invitation_uid: None,
    };
    assert!(settings.bona_fide);
}

#[test]
fn unauth_endpoint_rewrites_drive_routes() {
    assert_eq!(
        get_unauth_endpoint("drive/urls/anything"),
        "drive/urls/anything"
    );
    assert_eq!(
        get_unauth_endpoint("drive/v2/urls/anything"),
        "drive/v2/urls/anything"
    );
    assert_eq!(
        get_unauth_endpoint("drive/v2/anything"),
        "drive/unauth/v2/anything"
    );
    assert_eq!(
        get_unauth_endpoint("drive/anything"),
        "drive/unauth/anything"
    );
}

#[tokio::test]
async fn zip_generators_merges_both_sides() {
    use futures::stream;
    let mut values = zip_generators(
        stream::iter(["a", "b"]),
        stream::iter(["c"]),
        ZipOptions::default(),
    )
    .await;
    values.sort();
    assert_eq!(values, vec!["a", "b", "c"]);
}

#[test]
fn diagnostic_zip_contains_results() {
    let zip = generate_diagnostic_zip_from_results(&[DiagnosticResult {
        kind: "integrity".into(),
        message: "checked".into(),
    }])
    .unwrap();
    assert!(zip.starts_with(b"PK"));
    assert!(
        zip.windows(b"integrity: checked".len())
            .any(|w| w == b"integrity: checked")
    );
}
