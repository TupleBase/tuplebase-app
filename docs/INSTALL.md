# Install

## From a release

Download the file for your platform from
[the releases page](https://github.com/TupleBase/tuplebase-releases/releases/latest).

| Platform              | File                                 |
| --------------------- | ------------------------------------ |
| macOS (Apple Silicon) | `TupleBase-<version>-macos-arm64.dmg` |
| Windows (x64)         | `TupleBase-<version>-windows-x64.zip` |
| Linux (x64)           | `TupleBase-<version>-linux-x64.tar.gz` |

Builds are not code-signed yet, so the OS warns on first launch:

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
to x.y.z** button appears at the bottom of the sidebar and opens the download page.
Install the new version over the old one; connections and passwords are kept.

The check is one HTTPS request to GitHub. It sends no connection details or queries.
Untick **Check for updates** in the sidebar to turn it off.
