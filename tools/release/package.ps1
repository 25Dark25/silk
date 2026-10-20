<#
Builds the Windows release package from an existing release build:

    cargo build --locked --release
    powershell -File tools/release/package.ps1 [-Tag v0.1.0]

Writes dist/silk-<version>-windows-x64.zip (portable),
dist/silk-<version>-windows-x64.msi (per-user installer, installer/silk.wxs)
and dist/SHA256SUMS (LF endings, sha256sum format), and prints the hashes.
Needs cargo-about (with its "cli" feature), 7-Zip, WiX v7 (the "wix" dotnet
tool) and the Visual Studio C++ build tools, which provide the Visual C++
runtime DLLs. Every downloaded license file is checked against a pinned
SHA-256.
#>
param([string]$Tag = "")

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root

function Invoke-Native([string]$exe, [string[]]$arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) { throw "$exe failed with exit code $LASTEXITCODE" }
}

function Get-Sha256([string]$path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
}

function Get-Verified([string]$url, [string]$path, [string]$sha256) {
    Invoke-Native "curl.exe" @("-fsSL", "-o", $path, $url)
    $actual = Get-Sha256 $path
    if ($actual -ne $sha256) { throw "SHA-256 mismatch for ${url}: got $actual, expected $sha256" }
}

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
if ($Tag -and $Tag -ne "v$version") { throw "Tag $Tag does not match the crate version $version" }

$name = "silk-$version-windows-x64"
$dist = Join-Path $root "dist"
$stage = Join-Path $dist $name
$downloads = Join-Path $dist "downloads"
if (Test-Path -LiteralPath $dist) { Remove-Item -LiteralPath $dist -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $stage "models"), (Join-Path $stage "licenses"), $downloads | Out-Null

$sevenZip = (Get-Command 7z -ErrorAction SilentlyContinue).Source
if (-not $sevenZip) { $sevenZip = Join-Path $env:ProgramFiles "7-Zip\7z.exe" }

# The application, its model and the DirectML runtime copied by the ort crate.
Copy-Item target\release\silk.exe, target\release\DirectML.dll $stage
Copy-Item models\rife_v4.25_flow.onnx (Join-Path $stage "models")
Copy-Item LICENSE, README.md, tools\release\THIRD-PARTY.txt $stage

# DirectML: ship only the official file, with Microsoft's license and notices.
$nupkg = Join-Path $downloads "directml.zip"
Get-Verified "https://api.nuget.org/v3-flatcontainer/microsoft.ai.directml/1.15.4/microsoft.ai.directml.1.15.4.nupkg" $nupkg "4e7cb7ddce8cf837a7a75dc029209b520ca0101470fcdf275c1f49736a3615b9"
$nuget = Join-Path $downloads "directml"
Invoke-Native $sevenZip @("x", "-y", "-o$nuget", $nupkg, "LICENSE.txt", "ThirdPartyNotices.txt", "bin\x64-win\DirectML.dll") | Out-Null
if ((Get-Sha256 (Join-Path $stage "DirectML.dll")) -ne (Get-Sha256 (Join-Path $nuget "bin\x64-win\DirectML.dll"))) {
    throw "target\release\DirectML.dll is not the official DirectML 1.15.4 x64 file"
}
Copy-Item (Join-Path $nuget "LICENSE.txt") (Join-Path $stage "licenses\DirectML-LICENSE.txt")
Copy-Item (Join-Path $nuget "ThirdPartyNotices.txt") (Join-Path $stage "licenses\DirectML-ThirdPartyNotices.txt")

# ONNX Runtime 1.31.0 is linked into silk.exe; RIFE's weights are in the model.
$licenses = @(
    @("https://raw.githubusercontent.com/microsoft/onnxruntime/v1.31.0/LICENSE", "ONNX-Runtime-LICENSE.txt", "2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c"),
    @("https://raw.githubusercontent.com/microsoft/onnxruntime/v1.31.0/ThirdPartyNotices.txt", "ONNX-Runtime-ThirdPartyNotices.txt", "143764b952fdb1a7c69ce653bfba74a7744d6a8a573bfb73e235fba356c83de3"),
    @("https://raw.githubusercontent.com/hzwer/Practical-RIFE/7840e88fec61ada933b0d27775867b5851b64b44/LICENSE", "RIFE-LICENSE.txt", "7932fb49341512b959b1744a6d9cbb39e5a1ec89da438a34d0454d5d8df9fecd")
)
foreach ($l in $licenses) { Get-Verified $l[0] (Join-Path $stage "licenses\$($l[1])") $l[2] }

# The Rust crates compiled into silk.exe.
Invoke-Native "cargo" @("about", "generate", "--locked", "about.hbs", "-o", (Join-Path $stage "licenses\Rust-crates.txt"))

# The Visual C++ runtime, app-local: the DLLs silk.exe needs, and nothing
# else (they only import each other, kernel32 and the Universal CRT).
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio C++ build tools not found" }
$redist = Get-ChildItem -Directory (Join-Path $vs "VC\Redist\MSVC") | Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
$crt = Get-ChildItem -Directory (Join-Path $redist.FullName "x64") -Filter "Microsoft.VC*.CRT" | Select-Object -First 1
if (-not $crt) { throw "No Visual C++ runtime redist folder under $($redist.FullName)" }
foreach ($dll in "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_1.dll") {
    Copy-Item (Join-Path $crt.FullName $dll) $stage
}
Write-Host "Visual C++ runtime $((Get-Item (Join-Path $stage 'vcruntime140.dll')).VersionInfo.ProductVersion) from $($crt.FullName)"

Push-Location $dist
Invoke-Native $sevenZip @("a", "-tzip", "-mx=9", "$name.zip", $name) | Out-Null

# The installer, from the same staged files. Accepting WiX's OSMF EULA: its
# fee only applies to revenue-generating users above US$10,000 a year.
Invoke-Native "wix" @("build", "-acceptEula", "wix7", "-arch", "x64", "-d", "Version=$version", "-d", "PackageDir=$stage",
    (Join-Path $root "installer\silk.wxs"), "-o", "$name.msi")
# ICE91 only warns that per-user files would not reach every profile in a
# per-machine install; this package is per-user only.
Invoke-Native "wix" @("msi", "validate", "-acceptEula", "wix7", "-sice", "ICE91", "$name.msi")
Remove-Item "$name.wixpdb" -ErrorAction SilentlyContinue

$sums = "$(Get-Sha256 "$name.msi")  $name.msi`n$(Get-Sha256 "$name.zip")  $name.zip`n"
[IO.File]::WriteAllText((Join-Path $dist "SHA256SUMS"), $sums)
Pop-Location
Remove-Item -LiteralPath $downloads -Recurse -Force

Write-Host "Package contents:"
Get-ChildItem -Recurse -File $stage | ForEach-Object { Write-Host ("  {0,12:N0}  {1}" -f $_.Length, $_.FullName.Substring($stage.Length + 1)) }
Write-Host "SHA256SUMS:"
Write-Host -NoNewline $sums
