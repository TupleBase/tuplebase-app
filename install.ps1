# Install the latest TupleBase release for the current user.
#
#   irm https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.ps1 | iex
#
# Set $env:TUPLEBASE_VERSION = "x.y.z" to install a specific version.
# The app sets $env:TUPLEBASE_DOWNLOAD_DIR when it has already downloaded the release files.

# Everything runs in this block, so a truncated download cannot run half a script.
& {
    $ErrorActionPreference = "Stop"
    $ProgressPreference = "SilentlyContinue"

    $repo = "TupleBase/tuplebase-app"
    $dir = $env:TUPLEBASE_INSTALL_DIR
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA "Programs\TupleBase" }

    $version = $env:TUPLEBASE_VERSION
    if (-not $version) {
        $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name.TrimStart("v")
    }
    if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "no release found" }

    $file = "TupleBase-$version-windows-x64.zip"
    $base = "https://github.com/$repo/releases/download/v$version"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("tuplebase-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        if ($env:TUPLEBASE_DOWNLOAD_DIR) {
            Copy-Item (Join-Path $env:TUPLEBASE_DOWNLOAD_DIR $file) $tmp
            Copy-Item (Join-Path $env:TUPLEBASE_DOWNLOAD_DIR "SHA256SUMS") $tmp
        }
        else {
            Write-Host "Downloading TupleBase $version..."
            Invoke-WebRequest "$base/$file" -OutFile (Join-Path $tmp $file)
            Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")
        }

        $line = Select-String -Path (Join-Path $tmp "SHA256SUMS") -SimpleMatch $file
        $expected = ($line.Line -split '\s+')[0]
        $actual = (Get-FileHash (Join-Path $tmp $file) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "checksum mismatch for $file" }

        $new = Join-Path $tmp "new"
        Expand-Archive (Join-Path $tmp $file) -DestinationPath $new
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        # Copies an earlier install set aside, once nothing runs them any more.
        Get-ChildItem $dir -Filter "*.old-*" | Remove-Item -Force -ErrorAction SilentlyContinue

        # An agent may be running tuplebase-mcp.exe. Windows won't overwrite or delete a
        # running program, but it does rename one: each old file is renamed aside, then the
        # new one moved in. If that fails, the old files are put back.
        $aside = @()
        try {
            foreach ($item in Get-ChildItem $new) {
                $target = Join-Path $dir $item.Name
                if (Test-Path $target) {
                    $old = "$($item.Name).old-$([guid]::NewGuid())"
                    Rename-Item $target $old
                    $aside += [pscustomobject]@{ Target = $target; Old = (Join-Path $dir $old) }
                }
                Move-Item $item.FullName $target
            }
        }
        catch {
            foreach ($entry in $aside) {
                Remove-Item $entry.Target -Force -ErrorAction SilentlyContinue
                Move-Item $entry.Old $entry.Target -ErrorAction SilentlyContinue
            }
            throw
        }
        # One still running stays until the next install removes it.
        $aside | ForEach-Object { Remove-Item $_.Old -Force -ErrorAction SilentlyContinue }
    }
    finally {
        Remove-Item -Recurse -Force $tmp
    }

    $shortcut = Join-Path ([Environment]::GetFolderPath("Programs")) "TupleBase.lnk"
    $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
    $link.TargetPath = Join-Path $dir "tuplebase-app.exe"
    $link.Save()

    Write-Host "Installed app: $dir\tuplebase-app.exe (Start menu: TupleBase)"
    Write-Host "Installed MCP: $dir\tuplebase-mcp.exe"
}
