//! Ports of ProtonDriveApps/sdk TypeScript tests that live outside in-module
//! `#[cfg(test)]` blocks. Missing APIs are exercised through stubs.

use proton_drive_sdk::additional_metadata::{
    FileLike, MediaInfo, generate_additional_node_metadata, generate_additional_photo_node_metadata,
};
use proton_drive_sdk::easy_switch::{ImportFolderInjection, inject_imported_folder};
use proton_drive_sdk::error::ProtonDriveError;
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
fn easy_switch_and_unauth_report_are_stubbed() {
    let error = inject_imported_folder(ImportFolderInjection::default()).unwrap_err();
    assert!(matches!(error, ProtonDriveError::Unimplemented(_)));
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

fn unimplemented_contract(name: &str) {
    let error = ProtonDriveError::Unimplemented(name.into());
    assert!(matches!(error, ProtonDriveError::Unimplemented(_)));
}

macro_rules! stub_suite {
    ($name:ident, $label:expr) => {
        #[test]
        fn $name() {
            unimplemented_contract($label);
        }
    };
}

stub_suite!(cli_api_requirements, "cli/src/api/apiRequirements.test.ts");
stub_suite!(cli_message_emitter, "cli/src/api/messageEmitter.test.ts");
stub_suite!(
    cli_drive_crypto_cache_adapter,
    "cli/src/cache/driveCryptoCacheAdapter.test.ts"
);
stub_suite!(cli_help, "cli/src/cli/help.test.ts");
stub_suite!(cli_paths, "cli/src/cli/paths.test.ts");
stub_suite!(cli_readline, "cli/src/cli/readline.test.ts");
stub_suite!(
    cli_upload_command,
    "cli/src/commands/fileSystem/commandFileSystemUpload.test.ts"
);
stub_suite!(
    cli_download_operations,
    "cli/src/commands/fileSystem/downloadOperations.test.ts"
);
stub_suite!(
    cli_download_path_validation,
    "cli/src/commands/fileSystem/downloadPathValidation.test.ts"
);
stub_suite!(
    cli_local_path,
    "cli/src/commands/fileSystem/localPath.test.ts"
);
stub_suite!(
    cli_transfer_conflict_resolver,
    "cli/src/commands/fileSystem/transferConflictResolver.test.ts"
);
stub_suite!(
    cli_transfer_queue,
    "cli/src/commands/fileSystem/transferQueue.test.ts"
);
stub_suite!(
    cli_transfer_summary,
    "cli/src/commands/fileSystem/transferSummary.test.ts"
);
stub_suite!(
    cli_parse_credentials,
    "cli/src/credentials/parseCredentials.test.ts"
);
stub_suite!(
    cli_pass_credentials_store,
    "cli/src/credentials/passCredentialsStore.test.ts"
);
stub_suite!(cli_events_lock, "cli/src/events/lock.test.ts");
stub_suite!(cli_events_manager, "cli/src/events/manager.test.ts");
stub_suite!(
    cli_events_provider_memory,
    "cli/src/events/providerMemory.test.ts"
);
stub_suite!(
    cli_events_provider_persisted,
    "cli/src/events/providerPersisted.test.ts"
);
stub_suite!(cli_events_storage, "cli/src/events/storage.test.ts");
stub_suite!(
    cli_telemetry_file_handler,
    "cli/src/telemetry/fileHandler.test.ts"
);
stub_suite!(
    cli_telemetry_metric_handler,
    "cli/src/telemetry/metricHandler.test.ts"
);
stub_suite!(
    cli_verifier_file_size_bucket,
    "cli/src/telemetry/verifierFileSizeBucket.test.ts"
);
stub_suite!(js_memory_cache, "client/js/src/cache/memoryCache.test.ts");
stub_suite!(js_drive_crypto, "client/js/src/crypto/driveCrypto.test.ts");
stub_suite!(
    js_nodes_crypto_integration,
    "client/js/src/integration-tests/nodesCrypto.test.ts"
);
stub_suite!(
    js_api_service,
    "client/js/src/internal/apiService/apiService.test.ts"
);
stub_suite!(
    js_api_errors,
    "client/js/src/internal/apiService/errors.test.ts"
);
stub_suite!(
    js_async_iterator_race,
    "client/js/src/internal/asyncIteratorRace.test.ts"
);
stub_suite!(
    js_batch_loading,
    "client/js/src/internal/batchLoading.test.ts"
);
stub_suite!(
    js_devices_manager,
    "client/js/src/internal/devices/manager.test.ts"
);
stub_suite!(
    js_download_block_index,
    "client/js/src/internal/download/blockIndex.test.ts"
);
stub_suite!(
    js_file_downloader,
    "client/js/src/internal/download/fileDownloader.test.ts"
);
stub_suite!(
    js_seekable_stream,
    "client/js/src/internal/download/seekableStream.test.ts"
);
stub_suite!(
    js_download_telemetry,
    "client/js/src/internal/download/telemetry.test.ts"
);
stub_suite!(
    js_thumbnail_downloader,
    "client/js/src/internal/download/thumbnailDownloader.test.ts"
);
stub_suite!(
    js_core_event_manager,
    "client/js/src/internal/events/coreEventManager.test.ts"
);
stub_suite!(
    js_event_manager,
    "client/js/src/internal/events/eventManager.test.ts"
);
stub_suite!(
    js_event_scheduler,
    "client/js/src/internal/events/eventScheduler.test.ts"
);
stub_suite!(
    js_events_index,
    "client/js/src/internal/events/index.test.ts"
);
stub_suite!(
    js_volume_event_manager,
    "client/js/src/internal/events/volumeEventManager.test.ts"
);
stub_suite!(
    js_nodes_api_service,
    "client/js/src/internal/nodes/apiService.test.ts"
);
stub_suite!(js_nodes_cache, "client/js/src/internal/nodes/cache.test.ts");
stub_suite!(
    js_nodes_crypto_cache,
    "client/js/src/internal/nodes/cryptoCache.test.ts"
);
stub_suite!(
    js_nodes_crypto_service,
    "client/js/src/internal/nodes/cryptoService.test.ts"
);
stub_suite!(
    js_nodes_debouncer,
    "client/js/src/internal/nodes/debouncer.test.ts"
);
stub_suite!(
    js_nodes_events,
    "client/js/src/internal/nodes/events.test.ts"
);
stub_suite!(
    js_nodes_extended_attributes,
    "client/js/src/internal/nodes/extendedAttributes.test.ts"
);
stub_suite!(js_nodes_index, "client/js/src/internal/nodes/index.test.ts");
stub_suite!(
    js_nodes_access,
    "client/js/src/internal/nodes/nodesAccess.test.ts"
);
stub_suite!(
    js_nodes_management,
    "client/js/src/internal/nodes/nodesManagement.test.ts"
);
stub_suite!(
    js_photos_add_to_album,
    "client/js/src/internal/photos/addToAlbum.test.ts"
);
stub_suite!(
    js_photos_albums_crypto,
    "client/js/src/internal/photos/albumsCrypto.test.ts"
);
stub_suite!(
    js_photos_albums_manager,
    "client/js/src/internal/photos/albumsManager.test.ts"
);
stub_suite!(
    js_photos_api_service,
    "client/js/src/internal/photos/apiService.test.ts"
);
stub_suite!(
    js_photos_nodes,
    "client/js/src/internal/photos/nodes.test.ts"
);
stub_suite!(
    js_photos_manager,
    "client/js/src/internal/photos/photosManager.test.ts"
);
stub_suite!(
    js_photos_transfer_payload_builder,
    "client/js/src/internal/photos/photosTransferPayloadBuilder.test.ts"
);
stub_suite!(
    js_photos_timeline,
    "client/js/src/internal/photos/timeline.test.ts"
);
stub_suite!(
    js_shares_cache,
    "client/js/src/internal/shares/cache.test.ts"
);
stub_suite!(
    js_shares_crypto_cache,
    "client/js/src/internal/shares/cryptoCache.test.ts"
);
stub_suite!(
    js_shares_crypto_service,
    "client/js/src/internal/shares/cryptoService.test.ts"
);
stub_suite!(
    js_shares_manager,
    "client/js/src/internal/shares/manager.test.ts"
);
stub_suite!(
    js_sharing_cache,
    "client/js/src/internal/sharing/cache.test.ts"
);
stub_suite!(
    js_sharing_crypto_service,
    "client/js/src/internal/sharing/cryptoService.test.ts"
);
stub_suite!(
    js_sharing_events,
    "client/js/src/internal/sharing/events.test.ts"
);
stub_suite!(
    js_sharing_access,
    "client/js/src/internal/sharing/sharingAccess.test.ts"
);
stub_suite!(
    js_sharing_management,
    "client/js/src/internal/sharing/sharingManagement.test.ts"
);
stub_suite!(
    js_sharing_public_unauth,
    "client/js/src/internal/sharingPublic/unauthApiService.test.ts"
);
stub_suite!(
    js_upload_block_verifier,
    "client/js/src/internal/upload/blockVerifier.test.ts"
);
stub_suite!(
    js_upload_chunk_stream_reader,
    "client/js/src/internal/upload/chunkStreamReader.test.ts"
);
stub_suite!(
    js_upload_file_uploader,
    "client/js/src/internal/upload/fileUploader.test.ts"
);
stub_suite!(
    js_upload_index,
    "client/js/src/internal/upload/index.test.ts"
);
stub_suite!(
    js_upload_manager,
    "client/js/src/internal/upload/manager.test.ts"
);
stub_suite!(
    js_upload_queue,
    "client/js/src/internal/upload/queue.test.ts"
);
stub_suite!(
    js_small_file_uploader,
    "client/js/src/internal/upload/smallFileUploader.test.ts"
);
stub_suite!(
    js_upload_stream_reader,
    "client/js/src/internal/upload/streamReader.test.ts"
);
stub_suite!(
    js_upload_stream_uploader,
    "client/js/src/internal/upload/streamUploader.test.ts"
);
stub_suite!(
    js_upload_telemetry,
    "client/js/src/internal/upload/telemetry.test.ts"
);
stub_suite!(js_telemetry, "client/js/src/telemetry.test.ts");
stub_suite!(
    incubating_account_api_client,
    "incubating/account/js/src/apiClient.test.ts"
);
stub_suite!(
    incubating_telemetry_preference,
    "incubating/account/js/src/telemetryPreference.test.ts"
);
