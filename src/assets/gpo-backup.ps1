# PSCCM Group Policy backup: Backup-GPO -All into a temporary folder, then
# the folder as a zip (base64 in chunks) on standard output. Only reads AD.
# Lines: "@@domain<TAB>DNS name", "@@gpo<TAB>backup id<TAB>GPO GUID<TAB>name", "@@zip<TAB>chunk",
# "@@sha<TAB>SHA-256 of the zip", "@@done".
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
try { Import-Module GroupPolicy } catch {
    throw "the GroupPolicy PowerShell module is not installed here (it comes with the Group Policy Management feature / RSAT); run the GPO backup on a domain controller"
}
$dir = Join-Path ([IO.Path]::GetTempPath()) ('psccm-gpobk-' + [guid]::NewGuid().ToString('N'))
$zip = "$dir.zip"
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    $result = @(Backup-GPO -All -Path $dir -Comment 'PSCCM backup')
    if ($result.Count) { "@@domain`t" + $result[0].DomainName }
    foreach ($b in $result) {
        $name = ([string]$b.DisplayName) -replace "[`t`r`n]", ' '
        "@@gpo`t$($b.Id)`t$($b.GpoId)`t$name"
    }
    Compress-Archive -Path (Join-Path $dir '*') -DestinationPath $zip -Force
    $b64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($zip))
    for ($i = 0; $i -lt $b64.Length; $i += 16000) {
        "@@zip`t" + $b64.Substring($i, [Math]::Min(16000, $b64.Length - $i))
    }
    "@@sha`t" + (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    '@@done'
} finally {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
}
