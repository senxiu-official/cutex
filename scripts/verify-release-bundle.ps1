[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Bundle
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Bundle -PathType Container)) {
    throw "release bundle directory not found: $Bundle"
}

$required = @('cutex.exe', 'cute-codex.exe', 'codex-code-mode-host.exe')
foreach ($name in $required) {
    $artifact = Join-Path $Bundle $name
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) {
        throw "release bundle requires a regular executable: $artifact"
    }
    $item = Get-Item -LiteralPath $artifact
    if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "release bundle must be self-contained, not a reparse point: $artifact"
    }
}

& (Join-Path $Bundle 'cutex.exe') --version *> $null
if ($LASTEXITCODE -ne 0) { throw 'cutex.exe version smoke failed' }

& (Join-Path $Bundle 'cute-codex.exe') --version *> $null
if ($LASTEXITCODE -ne 0) { throw 'cute-codex.exe version smoke failed' }

& (Join-Path $Bundle 'codex-code-mode-host.exe') --help *> $null
if ($LASTEXITCODE -ne 0) { throw 'codex-code-mode-host.exe help smoke failed' }

Write-Output 'release bundle complete: cutex + cute-codex + codex-code-mode-host'
