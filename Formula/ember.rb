# Homebrew formula for ember — lightweight VM manager with copy-on-write storage.
#
# Install from tap:
#   brew tap aljoscha/ember https://github.com/aljoscha/ember
#   brew install ember
#
# Install latest development version:
#   brew install --HEAD aljoscha/ember/ember
class Ember < Formula
  desc "Lightweight VM manager with copy-on-write storage"
  homepage "https://github.com/aljoscha/ember"
  license "MIT"

  # Versioned release — updated by script/release.sh
  url "https://github.com/aljoscha/ember/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  # Development HEAD — always available
  head "https://github.com/aljoscha/ember.git", branch: "main"

  depends_on "rust" => :build
  depends_on "e2fsprogs"
  depends_on "skopeo"

  on_macos do
    depends_on xcode: ["14.0", :build]
    depends_on macos: :ventura
  end

  def install
    # Build the Rust CLI
    system "cargo", "install", *std_cargo_args

    # On macOS, build the Swift helper for Apple Virtualization Framework
    if OS.mac?
      cd "ember-vz" do
        system "swift", "build", "-c", "release",
               "--scratch-path", buildpath/"ember-vz/.build"
      end

      vz_bin = buildpath/"ember-vz/.build/release/ember-vz"

      # Code-sign with virtualization entitlement (required for AVF)
      system "codesign", "--force", "--sign", "-",
             "--entitlements", buildpath/"ember-vz/entitlements.plist",
             vz_bin

      bin.install vz_bin
    end
  end

  def caveats
    if OS.mac?
      <<~EOS
        ember uses Apple Virtualization Framework — no root required.

        Get started:
          ember init
          ember kernel build -y
          ember image build ubuntu-dev
          ember vm create myvm --image ubuntu-dev
          ember ssh myvm

        The kernel build enables Docker networking inside VMs.
        State is stored in ~/Library/Application Support/ember/
      EOS
    end
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ember version")
    if OS.mac?
      assert_match "USAGE", shell_output("#{bin}/ember-vz --help 2>&1")
    end
  end
end
