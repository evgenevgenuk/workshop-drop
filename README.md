<p align="center"><img src="assets/icon.png" width="128" alt="WorkshopDrop icon"></p>

# WorkshopDrop

**Paste a Steam Workshop link, and the addon gets installed on your Garry's Mod dedicated server.**

WorkshopDrop is a small Windows app for people who run their own GMod server. It downloads Workshop addons with steamcmd, extracts them into your server's `addons` folder, and makes sure joining players download them from the Workshop automatically.

No Lua knowledge, no `gmad.exe`, no manual unpacking.

## Features

- Paste one or many links (or plain ids). **Collections** work too: you get a checklist of every item.
- **Required items** from the Workshop page are detected and offered as extra checkboxes.
- Downloads several items in **one steamcmd run**, with live output in the log.
- Built-in **GMA extractor** (no gmad.exe), including old `_legacy.bin` (LZMA) items.
- Safe extraction: `..`, absolute paths and other unsafe paths are skipped, and CRC32 is checked for every file.
- Installs as `addons\ws_<id>_<name>`, for example `ws_104482086_precision_tool`.
- Creates a tiny **loader addon** (`addons\workshop_drop_loader`) that calls `resource.AddWorkshop(id)` for every `ws_<id>_*` folder, so players download the content when they join. You can turn this off in Settings.
- **Installed** tab: every installed addon with size and date, plus Open in Workshop, Update and Remove buttons.
- Warnings for maps (better added to your Workshop collection) and addons that look empty.
- Detects a running `srcds_win64.exe` and reminds you to restart. It never stops or restarts the server itself.

## Download

Get `WorkshopDrop.exe` from the [latest release](../../releases/latest). It is a single file and needs no installer or admin rights.

You also need:
- A Garry's Mod dedicated server (`srcds_win64.exe`)
- [steamcmd](https://developer.valvesoftware.com/wiki/SteamCMD)

## Usage

1. Run `WorkshopDrop.exe`. On first start, the **Settings** tab opens:
   - **Server folder**: the folder that contains `srcds_win64.exe`. The addons folder is filled in automatically.
   - **steamcmd folder**: the folder that contains `steamcmd.exe`.
   - Press **Save**. This creates `config.toml` next to the exe.
2. On the **Install** tab, paste links such as `https://steamcommunity.com/sharedfiles/filedetails/?id=104482086`, one per line, then press **Install to server**.
3. If there is anything to decide (dependencies, a collection, an existing version, a map), you get a checklist. Press **Install selected**.
4. Restart your server. Players will download the addon on join.

### Updating and removing

- **Update** downloads the newest version and replaces the old `ws_<id>_*` folder.
- **Remove** permanently deletes the addon folder, after you confirm.

Folders you created yourself named like `something_<id>` (without `ws_`) are detected, and WorkshopDrop offers to rename them to the proper format.

## Building from source

Requires Rust stable. On the GNU toolchain (`x86_64-pc-windows-gnu`) you also need MinGW-w64, for example `winget install BrechtSanders.WinLibs.POSIX.UCRT`. The MSVC toolchain works with Visual Studio Build Tools.

```bash
cargo build --release
cargo test
```

The exe ends up at `target\release\WorkshopDrop.exe`. On Windows, `build.cmd` sets up PATH for the GNU toolchain and builds.

Optional end-to-end test with network and steamcmd. It installs into a temporary folder, not your server:

```powershell
$env:WD_E2E_STEAMCMD = "C:\path\to\steamcmd"; $env:WD_E2E_IDS = "104482086"
cargo test e2e -- --ignored --nocapture
```

## How it works

| Module | Role |
|---|---|
| `config` | `config.toml` next to the exe |
| `steam_api` | link parsing, `GetPublishedFileDetails` and `GetCollectionDetails` (no API key needed), "Required items" scraping |
| `steamcmd` | `steamcmd +login anonymous +workshop_download_item 4000 <id> ... +quit` |
| `gma` | GMA reader/extractor, LZMA-alone decoder for `_legacy.bin` |
| `install` | planning, installing, the loader addon, the Installed list |
| `ui` | egui interface; all work runs on background threads |

If steamcmd reports "Success" but its cache has no files (a stale manifest), WorkshopDrop downloads the item again into its own temp folder. It never changes steamcmd's cache.

WorkshopDrop only writes to your server's `addons` folder and its own temp folder (`%TEMP%\workshop_drop`).

## License

[MIT](LICENSE)
