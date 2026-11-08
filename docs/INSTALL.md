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

This is the way to avoid the operating system's "unknown developer" warning. Builds are
not code-signed, and the warning only applies to files downloaded by a browser.

The script finds the latest release, downloads it, verifies it against `SHA256SUMS` and
installs it for the current user, without admin rights. You can read
[install.sh](../install.sh) and [install.ps1](../install.ps1) before running them.

| Platform | App                                                 | MCP server                                          |
| -------- | --------------------------------------------------- | --------------------------------------------------- |
| macOS    | `~/Applications/TupleBase.app`                      | `~/.local/bin/tuplebase-mcp`                        |
| Linux    | `~/.local/bin/tuplebase-app`, plus a menu entry     | `~/.local/bin/tuplebase-mcp`                        |
| Windows  | `%LOCALAPPDATA%\Programs\TupleBase\tuplebase-app.exe` | `%LOCALAPPDATA%\Programs\TupleBase\tuplebase-mcp.exe` |

To install a specific version, set `TUPLEBASE_VERSION=0.1.0` first.

## Browser download

Download the file for your platform from
[the releases page](https://github.com/TupleBase/tuplebase-app/releases/latest).

| Platform              | File                                   |
| --------------------- | -------------------------------------- |
| macOS (Apple Silicon) | `TupleBase-<version>-macos-arm64.dmg`  |
| Windows (x64)         | `TupleBase-<version>-windows-x64.zip`  |
| Linux (x64)           | `TupleBase-<version>-linux-x64.tar.gz` |

Browser downloads get the OS warning on first launch:

- **macOS** blocks the app. Allow it in System Settings → Privacy & Security → Open
  Anyway, or run `xattr -dr com.apple.quarantine /Applications/TupleBase.app`.
- **Windows** SmartScreen: More info → Run anyway.

## From source

macOS and Linux, needs the [prerequisites](DEVELOPMENT.md#prerequisites):

```sh
scripts/install.sh     # build and install for the current user, no sudo
scripts/uninstall.sh   # remove it again; config and passwords stay
```

| Platform | App                            | MCP server                  |
| -------- | ------------------------------ | --------------------------- |
| macOS    | `~/Applications/TupleBase.app` | `~/.local/bin/tuplebase-mcp` |
| Linux    | `~/.local/bin/tuplebase-app`   | `~/.local/bin/tuplebase-mcp` |

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
