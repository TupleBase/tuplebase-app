# Install the latest TupleBase release for the current user.
#
#   irm https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.ps1 | iex
#
# Set $env:TUPLEBASE_VERSION = "x.y.z" to install a specific version.

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
        Write-Host "Downloading TupleBase $version..."
        Invoke-WebRequest "$base/$file" -OutFile (Join-Path $tmp $file)
        Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")

        $line = Select-String -Path (Join-Path $tmp "SHA256SUMS") -SimpleMatch $file
        $expected = ($line.Line -split '\s+')[0]
        $actual = (Get-FileHash (Join-Path $tmp $file) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "checksum mismatch for $file" }

        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Expand-Archive (Join-Path $tmp $file) -DestinationPath $dir -Force
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
