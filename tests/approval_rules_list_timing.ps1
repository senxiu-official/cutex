param(
  [Parameter(Mandatory=$true)][string]$TestBinary,
  [string]$OutputPath = '.claude/approval-rules-fast-timing.log'
)
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path -LiteralPath $TestBinary).Path
if ((Split-Path -Leaf $binary) -notmatch '^cutex-[0-9a-f]+\.exe$' -or (Split-Path -Leaf (Split-Path -Parent $binary)) -ne 'deps' -or (Split-Path -Leaf (Split-Path -Parent (Split-Path -Parent $binary))) -ne 'debug') {
  throw 'Pass the debug lib test harness under target/debug/deps, never the installed cutex executable.'
}
$taskId = [guid]::NewGuid().ToString('N').Substring(0,8)
$privateHome = Join-Path $env:USERPROFILE ".cutex-rule-list-$taskId"
$privateTemp = "D:/ct261rule-$taskId"
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
foreach ($path in @($privateHome,$privateTemp)) {
  New-Item -ItemType Directory -Path $path | Out-Null
  $acl = Get-Acl -LiteralPath $path
  $acl.SetAccessRuleProtection($true,$false)
  $acl.SetOwner($sid)
  foreach ($principal in @($sid,[System.Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
    $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($principal,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
  }
  Set-Acl -LiteralPath $path -AclObject $acl
}
Set-Content -LiteralPath (Join-Path $privateHome '.cutex-test-private-home') -Value 'synthetic approval rule-list timing only'
$saved = @{}
$saved['CUTEX_HOME'] = [Environment]::GetEnvironmentVariable('CUTEX_HOME','Process')
[Environment]::SetEnvironmentVariable('CUTEX_HOME',$null,'Process')
$values = @{HOME=$privateHome;CUTEX_TEST_PRIVATE_HOME=$privateHome;CUTEX_TEST_FORBID_REAL_HOME='1';TEMP=$privateTemp;TMP=$privateTemp;TMPDIR=$privateTemp}
foreach ($name in $values.Keys) { $saved[$name]=[Environment]::GetEnvironmentVariable($name,'Process'); [Environment]::SetEnvironmentVariable($name,$values[$name],'Process') }
try {
  $parent = Split-Path -Parent $OutputPath
  if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
  & $binary --exact approval_rules::tests::benchmark_rule_list_large_history --ignored --nocapture --test-threads=1 2>&1 | Tee-Object -FilePath $OutputPath
  if ($LASTEXITCODE -ne 0) { throw "timing harness failed ($LASTEXITCODE)" }
  Write-Output "Private test home: $privateHome; private TEMP: $privateTemp"
} finally {
  foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name,$saved[$name],'Process') }
}
