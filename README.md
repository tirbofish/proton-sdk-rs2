# proton-sdk-rs2

A port of the [Proton SDK](https://github.com/ProtonDriveApps/sdk) (specifically the drive implementation) from c# to rust. 

This is the second iteration because the first iteration that I attempted to make was severly underperformant and my experience was poor. 
You can check it out [here](https://github.com/tirbofish/proton-sdk-rs)

> [!WARNING]
> This port is not an official product of Proton, nor is it made by Proton. It is a community project. 
> 
> Despite this project being open-source (and anyone can check the contents), there can be bugs and issues that may cause data loss, so always be aware of this issue. 
> 
> Passwords are not saved, but instead tokens are saved to the config. Even so, it is dependent on how the app (not the SDK) uses the tokens. There are helpers available for any developers wanting to store their credentials safely. 

# usage

## the normal joe
anyone wanting to use the client will have to clone this repository and compile with cargo (if no release has been made, or you just want cutting edge)

```bash
git clone https://github.com/tirbofish/proton-sdk-rs2
cd proton-sdk-rs2
cargo run
```

## sdk

proton-drive-sdk and proton-sdk-rs2 are both on crates.io, as well as the proton based cryptography libraries (no changes, just some cosmetic stuff). 

**crates.io**
```toml
proton-drive-sdk = { version = "0.1" }
```

**cutting edge**
```toml
proton-drive-sdk = { git = "https://github.com/tirbofish/proton-sdk-rs2" }
```

> [!NOTE]
> There is no need for you to include the `proton-sdk-rs2` library as part of your imports, it's already exported by `proton-drive-sdk`

# license

this project uses the MIT license because all the other proton-based repositories use MIT, and it would only be fair to use MIT myself. 

<details>
    <summary>Todo</summary>

### Core Operations
- [x] Authentication / Session management
- [x] Get My Files root folder
- [x] Get node by UID
- [x] Enumerate nodes
- [x] Create folder
- [x] Rename node
- [x] Move nodes
- [x] Copy node
- [x] Trash nodes
- [x] Restore nodes from trash
- [x] Delete nodes from trash
- [x] Empty trash
- [x] Enumerate trash
- [x] Get available name (collision handling)

### File Upload/Download
- [x] File upload (stream-based)
- [x] File revision upload
- [x] File download (stream-based)
- [x] Download to file path
- [x] Upload from file path
- [x] Thumbnails enumeration
- [x] Thumbnail fetch
- [ ] Upload pause/resume controller
- [ ] Download pause/resume controller
- [ ] Seekable stream for video playback
- [ ] Expected SHA1 verification on upload

### Revisions
- [x] Iterate revisions
- [x] Restore revision
- [x] Delete revision

### Devices (Computers/Backup)
- [x] List devices
- [x] Get device
- [x] Create device
- [x] Rename device
- [x] Delete device

### Events
- [x] Get volume latest event ID
- [x] Poll volume events
- [x] Get core latest event ID
- [x] Poll core events
- [ ] Subscribe to tree events
- [ ] Subscribe to drive events
- [ ] SDK events (TransfersPaused, TransfersResumed, RequestsThrottled)

### Sharing & Collaboration
- [ ] Share node (with users/public link)
- [ ] Unshare node
- [ ] Get sharing info (members, invitations, public link)
- [ ] Iterate nodes shared by me
- [ ] Iterate nodes shared with me
- [ ] Leave shared node
- [ ] Editors can share setting

### Invitations
- [ ] Iterate pending invitations
- [ ] Accept invitation
- [ ] Reject invitation
- [ ] Resend invitation email
- [ ] Convert non-Proton invitation

### Public Links
- [ ] Create public link (with password/expiration)
- [ ] Get public link info
- [ ] Authenticate public link
- [ ] Public link client for accessing shared content

### Bookmarks
- [ ] Iterate bookmarks
- [ ] Create bookmark
- [ ] Remove bookmark

### Photos
- [x] Photos client initialization
- [x] Get photos root folder
- [x] Photos file uploader
- [x] Photos file downloader
- [x] Enumerate timeline (basic)
- [ ] Timeline with pagination
- [ ] Create album
- [ ] Delete album
- [ ] Rename album
- [ ] Set album cover
- [ ] Add photos to album
- [ ] Remove photos from album
- [ ] Iterate album contents
- [ ] Favorite/unfavorite photo
- [ ] Photo tags (Favorites, Screenshots, Videos, LivePhotos, etc.)
- [ ] Duplicate detection

### Utilities
- [ ] Generate node UID from share/link IDs
- [ ] Get node web URL
- [ ] Get Docs key (for Proton Docs integration)

### Resilience & Error Handling
- [ ] Automatic retry with backoff
- [ ] Transfer queue management
- [ ] TooManyRequests handling
- [ ] Integrity exception types (ChecksumMismatch, ContentSizeMismatch, etc.)

### Telemetry
- [x] Telemetry trait/interface
- [ ] Upload/Download error events
- [ ] Block verification error events
- [ ] Decryption error events
</details>