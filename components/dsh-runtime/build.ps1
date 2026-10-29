#requires -Version 7.0
# Offline packager. Inputs are explicit staging trees; never runs npm or installs
# a runtime. The staged tree must match the target OS/arch (native npm addons).
param(
    [Parameter(Mandatory)][ValidateSet('windows-x64', 'linux-x64')][string]$Platform,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$OutDir,
    [Parameter(Mandatory)][string]$SourceTree
)
$ErrorActionPreference = 'Stop'
$source = (Resolve-Path -LiteralPath $SourceTree).Path
$adapter = if (Test-Path -LiteralPath (Join-Path $source 'components/dsh-runtime/component.json')) { Join-Path $source 'components/dsh-runtime' } else { $source }
$runtime = if (Test-Path -LiteralPath (Join-Path $adapter 'runtime')) { Join-Path $adapter 'runtime' } else { Join-Path $source '.dsh-install' }
$meta = Get-Content -Raw -LiteralPath (Join-Path $adapter 'component.json') | ConvertFrom-Json
if ($meta.name -ne 'dsh-runtime' -or $meta.version -ne $Version) { throw 'Component identity/version mismatch' }
$dsh = Get-Content -Raw -LiteralPath (Join-Path $runtime 'node_modules/@deepseek-ai/dsh/package.json') | ConvertFrom-Json
$ws = Get-Content -Raw -LiteralPath (Join-Path $runtime 'node_modules/ws/package.json') | ConvertFrom-Json
$catalog = Get-Content -Raw -LiteralPath (Join-Path $runtime 'node_modules/@deepseek-ai/dsh-session-format-catalog/package.json') | ConvertFrom-Json
if ($dsh.name -ne '@deepseek-ai/dsh' -or $dsh.version -ne '0.2.1-rc.3' -or $ws.name -ne 'ws' -or $ws.version -ne '8.22.0' -or $catalog.name -ne '@deepseek-ai/dsh-session-format-catalog' -or $catalog.version -ne '0.2.1-rc.3') { throw 'Expected DSH/catalog 0.2.1-rc.3 and ws 8.22.0' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$output = (Resolve-Path -LiteralPath $OutDir).Path
$stage = Join-Path $output ('.dsh-pack-' + [guid]::NewGuid().ToString('N'))
if ($stage.StartsWith($source + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Output must be outside source tree' }
New-Item -ItemType Directory -Path $stage | Out-Null
# Node's filesystem implementation handles long paths and bulk copying without
# tens of thousands of PowerShell provider invocations. It runs no package code.
$copyTool = Join-Path $stage '.copy-tree.mjs'
@'
import { cpSync, lstatSync } from 'node:fs';
import { basename } from 'node:path';
const [source, destination] = process.argv.slice(2);
cpSync(source, destination, { recursive: true, force: false, errorOnExist: true,
  filter(path) {
    if (basename(path) === '.bin') return false;
    if (lstatSync(path).isSymbolicLink()) throw new Error(`Links are forbidden: ${path}`);
    return true;
  },
});
'@ | Set-Content -Encoding utf8 -LiteralPath $copyTool
function Copy-Tree([string]$From, [string]$To) {
    & node $copyTool $From $To
    if ($LASTEXITCODE -ne 0) { throw 'Offline payload copy failed' }
}
foreach ($file in @('component.json','package.json','package-lock.json','src')) { Copy-Tree (Join-Path $adapter $file) (Join-Path $stage $file) }
New-Item -ItemType Directory -Path (Join-Path $stage 'runtime'), (Join-Path $stage 'node_modules') | Out-Null
foreach ($file in @('package.json','package-lock.json','node_modules')) { Copy-Tree (Join-Path $runtime $file) (Join-Path $stage "runtime/$file") }
Copy-Tree (Join-Path $runtime 'node_modules/ws') (Join-Path $stage 'node_modules/ws')
$nodeName = if ($Platform -eq 'windows-x64') { 'node.exe' } else { 'node' }
if (Test-Path -LiteralPath (Join-Path $runtime $nodeName)) { Copy-Tree (Join-Path $runtime $nodeName) (Join-Path $stage "runtime/$nodeName") }
foreach ($file in @('src/server.mjs','src/history-reader.mjs','src/runtime.mjs','src/rpc.mjs','src/delivery.mjs','src/dsh-plugin.mjs','src/session-ops.mjs','src/store.mjs','src/reconcile.mjs','src/nativeProjection.mjs','src/toolItems.mjs','runtime/node_modules/@deepseek-ai/dsh/lib/bin.js','runtime/node_modules/@deepseek-ai/dsh-session-format-catalog/lib/index.js','node_modules/ws/index.js')) {
    if (!(Test-Path -LiteralPath (Join-Path $stage $file) -PathType Leaf)) { throw "Required file missing: $file" }
}
$archive = Join-Path $output "component-dsh-runtime-$Version-$Platform.tar.gz"
if (Test-Path -LiteralPath $archive) { throw "Archive exists: $archive" }
& tar -czf $archive -C $stage component.json package.json package-lock.json src runtime node_modules
if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
"$hash  $([IO.Path]::GetFileName($archive))" | Set-Content -Encoding utf8 -LiteralPath (Join-Path $output "SHA256SUMS-component-dsh-runtime-$Platform")
@{ name='dsh-runtime'; version=$Version; platform=$Platform; dshVersion='0.2.1-rc.3'; nodeMinimum='24.9.0'; nodeMode=$(if (Test-Path (Join-Path $stage "runtime/$nodeName")) {'bundled'} else {'system'}); source=$source; sha256=$hash } | ConvertTo-Json | Set-Content -Encoding utf8 -LiteralPath (Join-Path $output "BUILD-INFO-component-dsh-runtime-$Platform.json")
# Keep the staged payload for review and direct --from-source use. No deletion,
# deployment selection, service restart or install-base mutation occurs.
Write-Output $archive
