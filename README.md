# proton-sdk-rs2

A port of the [Proton SDK](https://github.com/ProtonDriveApps/sdk) (specifically the drive implementation) from c# to rust. 

This is the second iteration because the first iteration that I attempted to make was severly underperformant and my experience was poor. 
You can check it out [here](https://github.com/tirbofish/proton-sdk-rs)

> [!WARNING]
> This port is not an official product of Proton, nor is it made by Proton. It is a community project. 
> 
> Despite this project being open-source (and anyone can check the contents), there can be bugs and issues that
> may cause data loss, so always be aware of this issue. 
> 
> Passwords are not saved, but instead tokens are saved to the config. Even so, it is dependent on how the app (not the SDK) uses the
> tokens. 

# todo

- [x] authentication
- caching
    - [x] in memory
    - [x] file cache (as .json)
    - [ ] sql cache
- [x] iterating through each file
- [ ] photos + albums (partial implementation right now)
- [x] downloads
- [x] uploads
- [x] thumbnails
- [ ] some good documentation
- [ ] events (doable, not in c# library but available in js lib)


<!-- if anyone is reading the comment of this repository:
yeah hi i previously had done a lot of commits but my dumbass decided to commit my pgp private key when i was doing one of my
tests so now it only looks like i have done one commit. fuck my fat chungus life... -->

# usage
proton-srp and other proton based cryptography libraries do not use crates.io, you will have to use this git repository as the latest kind. 

```toml
proton-drive = { git = "https://github.com/tirbofish/proton-sdk-rs2" }
```

i *might* consider uploading the proton-crypto based libraries up to crates.io if proton permits me to (or they can do themself idk). 

# license

this project uses the MIT license because all the other proton-based repositories use MIT, and it would only be fair to use MIT myself. 

i would like credit tho, like a link to this repository. no pressure tho :)