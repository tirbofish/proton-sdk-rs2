//! Easy Switch import helpers. Crypto material preparation is stubbed.

use crate::error::ProtonDriveError;
use crate::node::NodeUid;

#[derive(Debug, Clone, Default)]
pub struct ImportFolderInjection {
    pub parent_uid: Option<NodeUid>,
    pub encoded_passphrase: Option<String>,
}

pub fn prepare_import_crypto_material(
    _passphrase: &str,
) -> Result<ImportFolderInjection, ProtonDriveError> {
    Err(ProtonDriveError::Unimplemented(
        "Easy Switch import crypto material is not implemented".into(),
    ))
}

pub fn inject_imported_folder(
    _injection: ImportFolderInjection,
) -> Result<NodeUid, ProtonDriveError> {
    Err(ProtonDriveError::Unimplemented(
        "Easy Switch folder injection is not implemented".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_folder_injection_is_stubbed() {
        let error = inject_imported_folder(ImportFolderInjection::default()).unwrap_err();
        assert!(matches!(error, ProtonDriveError::Unimplemented(_)));
        let error = prepare_import_crypto_material("secret").unwrap_err();
        assert!(matches!(error, ProtonDriveError::Unimplemented(_)));
    }
}
