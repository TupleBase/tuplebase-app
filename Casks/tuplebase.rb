cask "tuplebase" do
  version "0.13.0"
  sha256 "79cfae9bdabf4c457d7361051d140d4ed378c73591213f07a885861790449f4b"

  url "https://github.com/TupleBase/tuplebase-app/releases/download/v#{version}/TupleBase-#{version}-macos-arm64.dmg"
  name "TupleBase"
  desc "Database client for PostgreSQL, MariaDB, MySQL, SQLite and Amazon Redshift"
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
