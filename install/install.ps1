# Install the latest (or a given) Veyra release on Windows.
#
#   irm https://raw.githubusercontent.com/RMV-Coder/veyra/main/install/install.ps1 | iex
#   $env:VEYRA_VERSION = "v1.0.0"; .\install.ps1
#
# The archive's SHA-256 is checked against the release's SHA256SUMS before anything is installed.
$ErrorActionPreference = "Stop"

$repo = "RMV-Coder/veyra"
$version = $env:VEYRA_VERSION
$dir = if ($env:VEYRA_INSTALL_DIR) { $env:VEYRA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\veyra" }
$target = "x86_64-pc-windows-msvc"

if (-not $version) {
    $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
$name = "veyra-$version-$target"
$base = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading Veyra $version for $target..."
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape("$name.zip"))$" }
    if (-not $line) { throw "no checksum for $name.zip; not installing" }
    $expected = ($line -split '\s+')[0]
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch; not installing" }
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        gh attestation verify "$tmp\$name.zip" --repo $repo | Out-Null
        if ($LASTEXITCODE -eq 0) { Write-Host "Provenance verified." }
    }
    Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item "$tmp\$name\veyra.exe" (Join-Path $dir "veyra.exe") -Force
    Write-Host "Installed $dir\veyra.exe"
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $dir })) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
        Write-Host "Added $dir to your user PATH (open a new terminal)."
    }
    & (Join-Path $dir "veyra.exe") --version
} finally {
    Remove-Item -Recurse -Force $tmp
}
