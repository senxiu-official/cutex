#requires -Version 7.0
# Source-only three-argument builder. r2 offline vendor trees are never repackaged.
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position=0)][ValidateSet('windows-x64','linux-x64')][string]$Platform,
    [Parameter(Mandatory, Position=1)][string]$Version,
    [Parameter(Mandatory, Position=2)][string]$OutDir
)
$ErrorActionPreference = 'Stop'
$builder = Join-Path $PSScriptRoot '../adapter_build.py'
if ($env:PYTHON) {
    & $env:PYTHON $builder dsh-runtime $Platform $Version $OutDir
} elseif (Get-Command py -ErrorAction SilentlyContinue) {
    & py -3 $builder dsh-runtime $Platform $Version $OutDir
} else {
    & python3 $builder dsh-runtime $Platform $Version $OutDir
}
if ($LASTEXITCODE -ne 0) { throw 'Source adapter build failed' }
