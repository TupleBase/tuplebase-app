# Install

## Command line (recommended)

macOS (Apple Silicon) and Linux (x64):

```sh
curl -fsSL https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.sh | sh
```

Windows (x64), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.ps1 | iex
```

This installs for the current user, without admin rights, and avoids the operating
system's "unknown developer" warning. See [details](#command-line-details).

## Browser download

Download the file for your platform from
[the releases page](https://github.com/TupleBase/tuplebase-app/releases/latest).

| Platform              | File                                   |
| --------------------- | -------------------------------------- |
| macOS (Apple Silicon) | `TupleBase-<version>-macos-arm64.dmg`  |
| Windows (x64)         | `TupleBase-<version>-windows-x64.zip`  |
| Linux (x64)           | `TupleBase-<version>-linux-x64.tar.gz` |

### macOS: "TupleBase" Not Opened

Builds are not code-signed, so macOS blocks the app the first time you open it. Allow it
once for each version you download with a browser. In-app updates don't need this.

1. Open TupleBase. When macOS says **"TupleBase" Not Opened**, click **Done**, not Move to
   Trash.
2. Open **System Settings**, go to **Privacy & Security**, and scroll down to **Security**.
3. Click **Open Anyway**. It shows for about an hour after step 1.

   ```text
   +----------------------------------------------------------------+
   | "TupleBase" was blocked to protect your Mac.   [ Open Anyway ] |  <-- click
   +----------------------------------------------------------------+
   | Apple could not verify "TupleBase" is free of malware that may |
   | harm your Mac or compromise your privacy.                      |
   +----------------------------------------------------------------+
   ```

4. When macOS asks, confirm with **Open Anyway** and your password or Touch ID.

Or replace these steps with one Terminal command:

```sh
xattr -dr com.apple.quarantine /Applications/TupleBase.app
```

Either way you turn off the macOS check for this app, so only do it for a file from
[the releases page](https://github.com/TupleBase/tuplebase-app/releases/latest).

### Windows: SmartScreen

If Windows warns on first launch, click **More info**, then **Run anyway**.

## Command line details

The script finds the latest release, downloads it, verifies it against `SHA256SUMS` and
installs it. You can read [install.sh](../install.sh) and [install.ps1](../install.ps1)
before running them. To install a specific version, set `TUPLEBASE_VERSION=0.1.0` first.

| Platform | App                                                 | MCP server                                          |
| -------- | --------------------------------------------------- | --------------------------------------------------- |
| macOS    | `~/Applications/TupleBase.app`                      | `~/.local/bin/tuplebase-mcp`                        |
| Linux    | `~/.local/bin/tuplebase-app`, plus a menu entry     | `~/.local/bin/tuplebase-mcp`                        |
| Windows  | `%LOCALAPPDATA%\Programs\TupleBase\tuplebase-app.exe` | `%LOCALAPPDATA%\Programs\TupleBase\tuplebase-mcp.exe` |

To build from source instead, see [Local development](DEVELOPMENT.md).

## Updates

At startup the app asks GitHub whether a newer version exists. If one does, an **Update
to x.y.z** button appears at the bottom of the sidebar. Click it and the update downloads
in the background while you keep working. When it's ready, **Restart to update** closes
TupleBase, installs the new version where the app already is, and reopens it. Connections
and passwords are kept. A log of the last update is saved as `update.log` next to
`connections.json`.

Running the install command again also updates to the latest version.

The check is one HTTPS request to GitHub. It sends no connection details or queries.
Untick **Check for updates** in the sidebar to turn it off.

## Uninstall

Connections and passwords are left in place; see [where they are stored](CREDENTIALS.md).

```sh
# macOS
rm -rf ~/Applications/TupleBase.app ~/.local/bin/tuplebase-mcp

# Linux
rm -f ~/.local/bin/tuplebase-app ~/.local/bin/tuplebase-mcp \
  ~/.local/share/applications/tuplebase.desktop \
  ~/.local/share/icons/hicolor/256x256/apps/tuplebase.png
rm -rf ~/.local/share/doc/tuplebase
```

```powershell
# Windows
Remove-Item -Recurse "$env:LOCALAPPDATA\Programs\TupleBase"
Remove-Item "$([Environment]::GetFolderPath('Programs'))\TupleBase.lnk"
```
