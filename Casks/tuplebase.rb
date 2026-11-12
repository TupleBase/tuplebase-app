cask "tuplebase" do
  version "0.3.0"
  sha256 "256028dd3597768f973f1f76ffb78662731fa294955480d1ee659692c9820a3f"

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
