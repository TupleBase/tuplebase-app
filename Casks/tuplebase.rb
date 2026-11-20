cask "tuplebase" do
  version "0.8.1"
  sha256 "20a8cee5cbb537488f7ca0c7ca35cef081b4eb532e1696a4184fd3b3e081f59b"

  url "https://github.com/TupleBase/tuplebase-app/releases/download/v#{version}/TupleBase-#{version}-macos-arm64.dmg"
  name "TupleBase"
  desc "Database client for PostgreSQL, MariaDB, MySQL and SQLite"
  homepage "https://tuplebase.app/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :monterey

  app "TupleBase.app"
  binary "#{appdir}/TupleBase.app/Contents/MacOS/tuplebase-mcp"

  # Builds are not notarized, so macOS would block the first launch.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/TupleBase.app"]
  end

  zap trash: "~/Library/Application Support/app.tuplebase.TupleBase"
end
