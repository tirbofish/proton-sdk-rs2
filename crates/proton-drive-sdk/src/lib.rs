//! start off at [`client::ProtonDriveClient`]

pub mod account;
pub mod api;
pub mod author;
pub mod block;
pub mod cache;
pub mod client;
pub mod crypto;
pub mod device_ops;
pub mod error;
pub mod http;
pub mod links;
pub mod memory;
pub mod meta;
pub mod node;
pub mod pgp;
pub mod revision;
pub mod share;
pub mod share_ops;
pub mod utils;
pub mod volume;
pub mod volume_operations;
pub mod photo;

pub use proton_sdk_rs2;

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/proton.drive.sdk.rs"));
}
