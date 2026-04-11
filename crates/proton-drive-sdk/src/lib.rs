//! The library to asynchronously interface with the ProtonDrive, with supports for many targets (such as Linux,
//! Windows, macOS and many other platforms (untested) ).
//!
//! # Example
//! ```
//! use proton_drive_sdk::client::ProtonDriveClient;
//! use proton_sdk_rs2::AppVersionConfiguration;
//! use proton_sdk_rs2::client::ProtonClientOptions;
//! use proton_sdk_rs2::session::ProtonAPISession;
//! use futures::StreamExt;
//! use std::pin::pin;
//!
//! #[tokio::main]
//! async fn main() {
//!     // creates an api session
//!     let session = ProtonAPISession::begin(
//!         "eric.nobert@acme.me",
//!         "password123",
//!         AppVersionConfiguration::new("example-proton-drive-app", 0, 1, 0),
//!         ProtonClientOptions::default(),
//!     ).await.unwrap();
//!
//!     // create a proton drive client that will let you talk to ProtonDrive
//!     let client = ProtonDriveClient::new(&session, None).unwrap();
//!
//!     // now you can do all your drive functions
//!     let folder_node = client.get_my_files_folder().await.unwrap();
//!
//!     let mut children = pin!(client.enumerate_folder_children(&folder_node).await.unwrap());
//!     while let Some(child) = children.next().await {
//!         let _node = child.unwrap();
//!     }
//! }
//! ```

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
