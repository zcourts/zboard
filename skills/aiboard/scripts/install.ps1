$ErrorActionPreference = "Stop"

$architecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture: $_" }
}

$installDir = if ($env:AIBOARD_INSTALL_DIR) {
    $env:AIBOARD_INSTALL_DIR
} else {
    Join-Path $HOME ".local\bin"
}
$asset = "aiboard-windows-$architecture.tar.gz"
$base = "https://github.com/zcourts/aiboard/releases/latest/download"
$temporaryDir = Join-Path ([System.IO.Path]::GetTempPath()) ("aiboard-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Path $temporaryDir | Out-Null
    $archive = Join-Path $temporaryDir $asset
    $checksums = Join-Path $temporaryDir "SHA256SUMS"
    Invoke-WebRequest "$base/$asset" -OutFile $archive
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile $checksums

    $line = Get-Content $checksums | Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $line) { throw "Release checksum does not contain $asset" }
    $expected = ($line -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "Checksum verification failed for $asset" }

    tar -xzf $archive -C $temporaryDir
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item (Join-Path $temporaryDir "aiboard.exe") (Join-Path $installDir "aiboard.exe") -Force
    Write-Output "Installed $(Join-Path $installDir 'aiboard.exe')"
    if (($env:PATH -split ";") -notcontains $installDir) {
        Write-Output "Add $installDir to PATH, then restart your agent client."
    }
} finally {
    if (Test-Path $temporaryDir) { Remove-Item -Recurse -Force $temporaryDir }
}
