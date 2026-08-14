use std::sync::LazyLock;

// 132 KiB to align and provide some padding for AEAD chunk size
// (128 KiB + PGP headers).
pub static MEMORY_STREAM_MANAGER: LazyLock<RecyclableMemoryStreamManager> = LazyLock::new(|| {
    RecyclableMemoryStreamManager::new(RecyclableMemoryStreamManagerOptions {
        block_size: 135_168,
    })
});

pub struct RecyclableMemoryStreamManagerOptions {
    pub block_size: usize,
}

pub struct RecyclableMemoryStreamManager {
    pub options: RecyclableMemoryStreamManagerOptions,
}

impl RecyclableMemoryStreamManager {
    pub fn new(options: RecyclableMemoryStreamManagerOptions) -> Self {
        Self { options }
    }
}
