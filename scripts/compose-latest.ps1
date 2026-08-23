param(
    [Parameter(Mandatory = $true)]
    [string] $Tag,

    [Parameter(Mandatory = $true)]
    [string] $BundleDir,

    [Parameter(Mandatory = $true)]
    [string] $OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "Tag must use vX.Y.Z format."
}

$installers = @(Get-ChildItem -LiteralPath $BundleDir -Filter '*.exe' -File)
if ($installers.Count -ne 1) {
    throw "Expected exactly one NSIS installer, found $($installers.Count)."
}

$installer = $installers[0]
$signaturePath = "$($installer.FullName).sig"
if (-not (Test-Path -LiteralPath $signaturePath -PathType Leaf)) {
    throw "NSIS installer signature is missing."
}

$version = $Tag.TrimStart('v')
$assetName = "StarVault.CCM_${version}_x64-setup.exe"
if ($assetName -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {
    throw "Published installer name contains characters GitHub may rewrite."
}

$publishedInstallerPath = Join-Path $BundleDir $assetName
$publishedSignaturePath = "$publishedInstallerPath.sig"
if ($installer.Name -ne $assetName) {
    if (
        (Test-Path -LiteralPath $publishedInstallerPath) -or
        (Test-Path -LiteralPath $publishedSignaturePath)
    ) {
        throw "Published installer or signature path already exists."
    }

    Move-Item -LiteralPath $installer.FullName -Destination $publishedInstallerPath
    try {
        Move-Item -LiteralPath $signaturePath -Destination $publishedSignaturePath
    }
    catch {
        Move-Item -LiteralPath $publishedInstallerPath -Destination $installer.FullName
        throw
    }
}
else {
    $publishedInstallerPath = $installer.FullName
    $publishedSignaturePath = $signaturePath
}

$signature = (Get-Content -LiteralPath $publishedSignaturePath -Raw).Trim()
$encodedAssetName = [System.Uri]::EscapeDataString($assetName)
$manifest = @{
    version = $version
    notes = "See https://github.com/uepoch/starvault-ccm/releases/tag/$Tag"
    pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = @{
        "windows-x86_64" = @{
            signature = $signature
            url = "https://github.com/uepoch/starvault-ccm/releases/download/$Tag/$encodedAssetName"
        }
    }
}

$json = $manifest | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText(
    $OutputPath,
    $json,
    [System.Text.UTF8Encoding]::new($false)
)
