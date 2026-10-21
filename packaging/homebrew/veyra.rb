# Homebrew formula template. The release workflow publishes the archives and SHA256SUMS that the
# placeholders refer to; update VERSION and the checksums from SHA256SUMS for each release
# (or point a tap's bump action at the release).
class Veyra < Formula
  desc "Git-compatible version control with undo, checkpoints and code intelligence"
  homepage "https://github.com/RMV-Coder/veyra"
  version "VERSION"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/RMV-Coder/veyra/releases/download/vVERSION/veyra-vVERSION-aarch64-apple-darwin.tar.gz"
      sha256 "SHA256_AARCH64_APPLE_DARWIN"
    end
    on_intel do
      url "https://github.com/RMV-Coder/veyra/releases/download/vVERSION/veyra-vVERSION-x86_64-apple-darwin.tar.gz"
      sha256 "SHA256_X86_64_APPLE_DARWIN"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/RMV-Coder/veyra/releases/download/vVERSION/veyra-vVERSION-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_AARCH64_LINUX"
    end
    on_intel do
      url "https://github.com/RMV-Coder/veyra/releases/download/vVERSION/veyra-vVERSION-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_X86_64_LINUX"
    end
  end

  def install
    bin.install "veyra"
  end

  test do
    system bin/"veyra", "init", testpath/"repo"
    assert_match "On branch", shell_output("#{bin}/veyra -C #{testpath}/repo status")
  end
end
