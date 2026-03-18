# proton-sdk-rs2

A port of the Proton SDK (specifically the drive implementation) from c# to rust. 

> [!WARNING]
> This port is not an official product of Proton, nor is it made by Proton. It is a community project. 
> 
> Despite this project being open-source (and anyone can check the contents), there can be bugs and issues that
> may cause data loss. 
> 
> Passwords are not saved, but instead tokens are saved to the config. 

# todo

- [x] authentication
- caching
    - [x] in memory
    - [x] file cache (as .json)
    - [ ] sql cache
- [x] iterating through each file
- [ ] photos + albums (partial implementation right now)
- [x] downloads
- [ ] uploads
- [ ] thumbnails
- [ ] events (doable, not in c# library but available in js lib)
