Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-Equal {
    param(
        [Parameter(Mandatory = $true)] $Expected,
        [Parameter(Mandatory = $true)] $Actual,
        [Parameter(Mandatory = $true)] [string] $Message
    )

    if ($Expected -ne $Actual) {
        throw "$Message Expected '$Expected', got '$Actual'."
    }
}

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) "svccm-compose-latest-$([guid]::NewGuid())"
$bundleDir = Join-Path $testRoot "bundle/nsis"
$outputPath = Join-Path $testRoot "bundle/latest.json"
$scriptPath = Join-Path $PSScriptRoot "compose-latest.ps1"

try {
    New-Item -ItemType Directory -Path $bundleDir | Out-Null
    Set-Content -LiteralPath (Join-Path $bundleDir "StarVault CCM_1.2.3_x64-setup.exe") -Value "installer"
    Set-Content -LiteralPath (Join-Path $bundleDir "StarVault CCM_1.2.3_x64-setup.exe.sig") -Value "test-signature"

    & $scriptPath -Tag "v1.2.3" -BundleDir $bundleDir -OutputPath $outputPath

    $expectedName = "StarVault.CCM_1.2.3_x64-setup.exe"
    $expectedUrl = "https://github.com/uepoch/starvault-ccm/releases/download/v1.2.3/$expectedName"
    $manifest = Get-Content -LiteralPath $outputPath -Raw | ConvertFrom-Json
    $installerNames = @(
        Get-ChildItem -LiteralPath $bundleDir -Filter '*.exe' -File |
            Select-Object -ExpandProperty Name
    )

    Assert-Equal 1 $installerNames.Count "The composer must leave exactly one installer."
    Assert-Equal $expectedName $installerNames[0] "The installer must use the published asset name."
    Assert-Equal $expectedUrl $manifest.platforms.'windows-x86_64'.url "The manifest URL must name that installer."
    Assert-Equal $true (Test-Path -LiteralPath (Join-Path $bundleDir "$expectedName.sig") -PathType Leaf) "The matching signature must exist."
    Assert-Equal "test-signature" $manifest.platforms.'windows-x86_64'.signature "The manifest must retain the signature."
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
