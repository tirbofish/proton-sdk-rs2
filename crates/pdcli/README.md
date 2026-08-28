# pdcli

Mount your Proton Drive to a FUSE filesystem.

## Usage

```
pdcli              # GUI (on WSL: mount instead)
pdcli gui          # graphical app
pdcli login        # browser sign-in
pdcli logout       # sign out and unmount
pdcli mount        # sign in if needed, mount ~/ProtonDrive
pdcli status       # login + daemon state
pdcli stop         # unmount and stop
pdcli pause        # pause background sync
pdcli resume       # resume background sync
pdcli sync         # retry sync now
pdcli open         # open the Drive folder
pdcli computers    # list computers and sync jobs
pdcli computers register [--name NAME] [--bind DEVICE_ID]
pdcli computers sync ~/Documents [--name Documents]
pdcli computers restore <computer> <folder> ~/Documents
pdcli computers unsync Documents
pdcli --help
```

The mount point is `~/ProtonDrive`. Run `pdcli gui` on WSL if you want the window.

## Disclaimer

> [!NOTE]
> I admit that this crate (pdcli) was AI generated. This app is not vibe-coded, but rather meticulously reviewed through with my advice, and safety, security and privacy has been considered+implemented in all steps. 
> 
> I have reviewed and approved/declined each step, and made sure it was tailored to be a nice and smooth
> experience, however I am giving you this information now to make sure you are aware. 
> 
> If you would not like to use this app due to its AI usage, feel free to develop your own app with  `proton-drive-sdk`. 