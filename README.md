# VibeSpace

What is eating your disk, in seconds.

Pick a drive or a folder: every folder is sized, the big ones are drawn to scale in a map, and
cleanup goes to the Recycle Bin, never straight to oblivion. A big folder can move to another
drive, with every file checked, and a link left at its old place so programs still find it. No
admin rights, no nags, no ads.

## What it does

- **Fast scans without admin rights:** folders are read in bulk, in parallel. Hard links count
  once, cloud placeholders (OneDrive "online only") are never opened or downloaded, and junctions
  and symlinks are listed but never followed.
- **Size and size on disk,** side by side, so compressed and sparse files show the difference
  honestly.
- **The tree, the map, the types and the largest files:** an expandable folder tree sorted by
  size, a treemap of the selected folder, totals by file type, and the 1000 largest files.
- **Cleanup:** Move to Recycle Bin, with the total in the confirmation. Never a permanent delete.
- **Move to another drive and leave a link:** the folder is copied, every file is checked, and a
  directory junction takes its old place. Until the move is done, either the original or a
  complete, checked copy is whole, with a journal that says how to get back. Move back undoes it.

## What it touches on your PC

Only what you act on: files you send to the Recycle Bin, and folders you move. Its settings live in
`%APPDATA%\com.tedkj.vibespace`, and a move's journal in `%LOCALAPPDATA%\com.tedkj.vibespace`.
Export copies a listing as text or saves the top folders and files as CSV. The app never needs or
asks for administrator rights.

## What it connects to

Nothing, unless you press About's "Check this install". That fetches the signed release list from
getvibeware.com to compare your copy with the published release. To check the installer's code
signature, Windows itself may fetch certificates from Windows Update and Certum (who issued the
certificate).

## Download

Get the installer from [Releases](https://github.com/tedkj/vibespace/releases) or
[getvibeware.com](https://getvibeware.com). It installs for your user only, without admin rights.
The installer, VibeSpace.exe and its uninstaller are code signed by "Open Source Developer Ted
Kenneth Johansen" (right-click › Properties › Digital Signatures). Compare the installer's SHA-256
with the one in `SHA256SUMS.txt` on the release page:

```powershell
Get-FileHash .\VibeSpace_0.1.0-preview_x64-setup.exe
```

## Building from source

You need a current stable Rust toolchain on Windows (the MSVC target).

```sh
cargo build --release --locked
```

The app is then at `target\release\vibespace.exe`.

### Building the installer

`installer\build.ps1` builds the app and its installer,
`target\release\bundle\VibeSpace_<version>_x64-setup.exe`. It needs the official NSIS 3.13:
download `nsis-3.13.zip` from SourceForge (SHA-256
`ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8`) and unpack it to
`_tools\nsis-3.13` in the folder that holds this repository (`..\_tools\nsis-3.13`). The script
checks that whole folder against a pinned hash before it runs anything from it.
`installer\notices.ps1` writes `THIRD-PARTY-NOTICES.txt` for each build (every crate the app
links, with its licence text), and the installer puts it and `LICENSE` next to the app.

## License

[AGPL-3.0](LICENSE) (AGPL-3.0-only). Use it, change it, share it. If you hand out a changed
version, or run one as a service for other people, you share your changes under the same license.
Slint is used under its GPLv3 option. `vendor/` holds Slint's `i-slint-core` 1.18.1 with one small
repaint fix; `vendor/README.md` says what changed and why.

Made in Norway by Ted K. Johansen.
