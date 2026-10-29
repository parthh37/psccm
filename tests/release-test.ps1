<#
.SYNOPSIS
    End-to-end release test for PSCCM. Writes report.html, report.json and junit.xml.

.DESCRIPTION
    Runs against a built or installed PSCCM and, optionally, real target computers:
      - installer: silent install, Windows service, upgrade in place, uninstall   (-Setup)
      - binary:    version, help, built-in packages
      - API:       auth, packages, computers, groups, inventory, credentials, schedules,
                   deployments, redeploy, cancel, updates, GPO backup import
      - Windows:   WinRM commands, inventory scan, every step type, rollback, detect, GPO
                   apply/remove, rolling batches, real installs (7-Zip, Node.js, IIS)
      - Linux:     SSH + sudo, inventory scan, every step type, rollback, real installs
                   (7-Zip, Node.js, HAProxy upgrade/config/drain)
    Tests that need something not provided are reported as "skip".

    -RealInstalls installs real software and changes the targets (registry
    policies, IIS, HAProxy). Use it on test machines only (CI does).

.EXAMPLE
    ./tests/release-test.ps1 -Exe .\target\release\psccm.exe
.EXAMPLE
    ./tests/release-test.ps1 -Setup .\dist\psccm-setup-0.2.0.exe -ExpectedVersion 0.2.0 -WindowsTarget localhost -RealInstalls
.EXAMPLE
    pwsh ./tests/release-test.ps1 -Exe ./psccm-x86_64-linux -LinuxTarget 10.0.0.5 -LinuxUser deploy -LinuxPassword ...
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [string]$Setup,
    [string]$ExpectedVersion,
    [string]$WindowsTarget,
    [string]$WindowsUser,
    [string]$WindowsPassword,
    [string]$LinuxTarget,
    [string]$LinuxUser,
    [string]$LinuxPassword,
    [string]$LinuxKey,
    [switch]$RealInstalls,
    [switch]$AdTests,
    [string]$UpdateRepo,
    [string]$ReportDir = (Join-Path (Get-Location) 'test-report')
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$OnWindows = [Environment]::OSVersion.Platform -eq 'Win32NT'
$Started = Get-Date
$Results = New-Object System.Collections.ArrayList
New-Item -ItemType Directory -Force -Path $ReportDir | Out-Null
$ReportDir = (Resolve-Path $ReportDir).Path

# ------------------------------------------------------------------ helpers

function Skip([string]$Why) { throw "SKIP: $Why" }
function Assert($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }

function Test-Step([string]$Group, [string]$Name, [scriptblock]$Body) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $status = 'pass'; $detail = ''
    try {
        $out = & $Body
        if ($null -ne $out) { $detail = ($out | Out-String).Trim() }
    } catch {
        $msg = $_.Exception.Message
        if ($msg -like 'SKIP:*') { $status = 'skip'; $detail = $msg.Substring(5).Trim() }
        else { $status = 'fail'; $detail = $msg }
    }
    $sw.Stop()
    [void]$Results.Add([pscustomobject]@{ group = $Group; name = $Name; status = $status; ms = [int]$sw.ElapsedMilliseconds; detail = $detail })
    $color = @{ pass = 'Green'; fail = 'Red'; skip = 'Yellow' }[$status]
    Write-Host ("[{0}] {1} / {2} ({3} ms)" -f $status.ToUpper(), $Group, $Name, $sw.ElapsedMilliseconds) -ForegroundColor $color
    if ($status -eq 'fail') { Write-Host "       $detail" -ForegroundColor DarkRed }
}

# Run psccm; stderr is merged into the output without PowerShell 5.1 turning it into errors.
function Invoke-Psccm([string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $out = @(& $script:Exe @Arguments 2>&1 | ForEach-Object { "$_" }) } finally { $ErrorActionPreference = $old }
    $script:ExitCode = $LASTEXITCODE
    $out
}

function ConvertFrom-JsonSafe([string]$Text) {
    if ([string]::IsNullOrWhiteSpace($Text)) { return $null }
    try { return $Text | ConvertFrom-Json } catch { return $Text }
}

function Api([string]$Method, [string]$Path, $Body = $null, [switch]$NoAuth, [switch]$Raw) {
    $headers = @{}
    if (-not $NoAuth -and $script:Token) { $headers.Authorization = "Bearer $script:Token" }
    $p = @{ Method = $Method; Uri = "$script:Base/api$Path"; Headers = $headers; UseBasicParsing = $true; TimeoutSec = 300 }
    if ($null -ne $Body) {
        $p.Body = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $Body -Depth 20 -Compress))
        $p.ContentType = 'application/json; charset=utf-8'
    }
    try {
        $r = Invoke-WebRequest @p
        $content = if ($r.Content -is [byte[]]) { [Text.Encoding]::UTF8.GetString($r.Content) } else { [string]$r.Content }
        $b = if ($Raw) { $content } else { ConvertFrom-JsonSafe $content }
        return [pscustomobject]@{ Status = [int]$r.StatusCode; Body = $b }
    } catch {
        $resp = $_.Exception.Response
        if ($null -eq $resp) { throw }
        return [pscustomobject]@{ Status = [int]$resp.StatusCode; Body = (ConvertFrom-JsonSafe $_.ErrorDetails.Message) }
    }
}

function Ok($Response) {
    if ($Response.Status -ge 300) {
        $e = if ($Response.Body -and $Response.Body.error) { $Response.Body.error } else { $Response.Body }
        throw "HTTP $($Response.Status): $e"
    }
    $Response.Body
}

function Wait-Deployment([string]$Id, [int]$TimeoutSec = 1800) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    do {
        Start-Sleep -Milliseconds 1500
        $d = Ok (Api GET "/deployments/$Id")
    } while ($d.deployment.status -in @('queued', 'running') -and (Get-Date) -lt $deadline)
    $logs = @{}
    foreach ($t in @($d.targets)) { $logs[$t.computer] = [string](Ok (Api GET "/targets/$($t.id)/log" -Raw)) }
    [pscustomobject]@{
        Id = $Id; Status = $d.deployment.status; Targets = @($d.targets); Logs = $logs
        AllLogs = ($logs.Values -join "`n"); Deployment = $d.deployment
    }
}

# Scan computers' inventory and return each one's stored inventory (by name).
function Invoke-Scan([string[]]$Names, [int]$TimeoutSec = 600) {
    [void](Ok (Api POST '/inventory/scan' @{ names = $Names }))
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    do {
        Start-Sleep -Milliseconds 1500
        $r = Ok (Api GET '/inventory')
    } while ($r.scan.running -and (Get-Date) -lt $deadline)
    Assert (-not $r.scan.running) 'inventory scan did not finish'
    $out = @{}
    foreach ($n in $Names) { $out[$n] = @($r.inventory) | Where-Object { $_.computer -eq $n } }
    $out
}

function Invoke-Deploy([hashtable]$Body, [int]$TimeoutSec = 1800) {
    $r = Ok (Api POST '/deployments' $Body)
    Wait-Deployment $r.id $TimeoutSec
}

function Assert-Deploy($Result, [string]$Status, [string[]]$Contains = @(), [string[]]$NotContains = @()) {
    $problems = @()
    if ($Result.Status -ne $Status) { $problems += "status is '$($Result.Status)', expected '$Status'" }
    foreach ($c in $Contains) { if ($Result.AllLogs -notlike "*$c*") { $problems += "log does not contain '$c'" } }
    foreach ($c in $NotContains) { if ($Result.AllLogs -like "*$c*") { $problems += "log must not contain '$c'" } }
    if ($problems) {
        $errs = ($Result.Targets | ForEach-Object { "$($_.computer): $($_.status) $($_.error)" }) -join '; '
        $log = $Result.AllLogs
        if ($log.Length -gt 4000) { $log = '...' + $log.Substring($log.Length - 4000) }
        throw (($problems -join '; ') + "`nTargets: $errs`n--- log ---`n$log")
    }
    "deployment $($Result.Id.Substring(0, 8)): $($Result.Status)"
}

function Add-Package([string]$Yaml) {
    $id = ([regex]::Match($Yaml, '(?m)^id:\s*(\S+)')).Groups[1].Value
    if ((Api GET "/packages/$id").Status -eq 200) { [void](Ok (Api PUT "/packages/$id" @{ yaml = $Yaml })) }
    else { [void](Ok (Api POST '/packages' @{ yaml = $Yaml })) }
    $id
}

function New-PolBytes([object[]]$Entries) {
    $ms = New-Object IO.MemoryStream
    $w = New-Object IO.BinaryWriter($ms)
    $u = [Text.Encoding]::Unicode
    $w.Write([byte[]][char[]]'PReg'); $w.Write([uint32]1)
    foreach ($e in $Entries) {
        $w.Write($u.GetBytes('[' + $e.Key)); $w.Write([uint16]0)
        $w.Write($u.GetBytes(';' + $e.Name)); $w.Write([uint16]0)
        $w.Write($u.GetBytes(';')); $w.Write([uint32]$e.Type)
        $w.Write($u.GetBytes(';')); $w.Write([uint32]$e.Data.Length)
        $w.Write($u.GetBytes(';')); $w.Write([byte[]]$e.Data)
        $w.Write($u.GetBytes(']'))
    }
    $w.Flush(); $ms.ToArray()
}

function Write-Utf8([string]$Path, [string]$Text) {
    New-Item -ItemType Directory -Force -Path (Split-Path $Path) | Out-Null
    [IO.File]::WriteAllText($Path, $Text, (New-Object Text.UTF8Encoding $false))
}

# ---------------------------------------------------------------- installer

$InstalledExe = if ($OnWindows) { Join-Path $env:ProgramFiles 'PSCCM\psccm.exe' } else { $null }

function Wait-Url([string]$Url, [int]$Seconds = 60) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        try { Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 5 | Out-Null; return $true } catch {
            if ($_.Exception.Response) { return $true }
        }
        Start-Sleep -Milliseconds 700
    }
    $false
}

if ($Setup) {
    Test-Step 'Installer' 'Silent install' {
        if (-not $OnWindows) { Skip 'installer is Windows only' }
        $p = Start-Process -FilePath $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$ReportDir\install.log" -Wait -PassThru
        Assert ($p.ExitCode -eq 0) "setup exit code $($p.ExitCode)"
        Assert (Test-Path $InstalledExe) "$InstalledExe missing"
        Assert (Test-Path (Join-Path $env:ProgramFiles 'PSCCM\unins000.exe')) 'uninstaller missing'
    }
    Test-Step 'Installer' 'Service installed, automatic and running' {
        if (-not $OnWindows) { Skip 'Windows only' }
        $s = Get-Service PSCCM
        Assert ($s.Status -eq 'Running') "service is $($s.Status)"
        Assert ("$($s.StartType)" -eq 'Automatic') "start type is $($s.StartType)"
    }
    Test-Step 'Installer' 'Service console answers on :8080' {
        if (-not $OnWindows) { Skip 'Windows only' }
        Assert (Wait-Url 'http://127.0.0.1:8080/api/summary' 60) 'no answer on http://127.0.0.1:8080'
        $html = (Invoke-WebRequest -Uri 'http://127.0.0.1:8080/' -UseBasicParsing).Content
        Assert ($html -like '*PSCCM*') 'console page does not look right'
        Assert (Test-Path (Join-Path $env:ProgramData 'PSCCM\psccm.db')) 'database not created in ProgramData'
    }
    Test-Step 'Installer' 'Upgrade in place keeps the service running' {
        if (-not $OnWindows) { Skip 'Windows only' }
        $p = Start-Process -FilePath $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$ReportDir\upgrade.log" -Wait -PassThru
        Assert ($p.ExitCode -eq 0) "setup exit code $($p.ExitCode)"
        Start-Sleep -Seconds 3
        Assert ((Get-Service PSCCM).Status -eq 'Running') 'service not running after upgrade'
        Assert (Wait-Url 'http://127.0.0.1:8080/api/summary' 60) 'console did not come back after upgrade'
    }
    if (-not $Exe -and (Test-Path $InstalledExe)) { $Exe = $InstalledExe }
}

if (-not $Exe) {
    $candidates = @('target/release/psccm.exe', 'target/release/psccm', $InstalledExe) | Where-Object { $_ -and (Test-Path $_) }
    $Exe = @($candidates)[0]
}
if (-not $Exe) { throw 'psccm executable not found; pass -Exe or -Setup' }
$Exe = (Resolve-Path $Exe).Path
Write-Host "Testing $Exe" -ForegroundColor Cyan

# ------------------------------------------------------------------- binary

$Data = Join-Path ([IO.Path]::GetTempPath()) ("psccm-rt-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $Data | Out-Null
$Version = ''

Test-Step 'Binary' 'Version' {
    $out = (& $Exe --version) -join ' '
    $script:Version = ($out -split ' ')[-1]
    if ($ExpectedVersion) { Assert ($script:Version -eq $ExpectedVersion) "version is '$out', expected $ExpectedVersion" }
    $out
}
Test-Step 'Binary' 'Help lists the commands' {
    $out = (& $Exe --help) -join "`n"
    foreach ($c in 'serve', 'deploy', 'run', 'packages', 'computers', 'groups', 'inventory', 'gpo', 'update', 'service') {
        Assert ($out -match "(?m)^\s+$c\b") "help does not mention '$c'"
    }
}
Test-Step 'Binary' 'Built-in packages load' {
    $out = Invoke-Psccm @('--data-dir', $Data, 'packages', 'list')
    $text = ($out | Out-String)
    Assert ($script:ExitCode -eq 0) "exit code $script:ExitCode"
    Assert ($text -notmatch 'BROKEN') $text
    $n = @($out | Where-Object { "$_".Trim() }).Count
    Assert ($n -ge 17) "only $n packages"
    "$n packages"
}
Test-Step 'Binary' 'Package validation rejects mistakes' {
    $bad = Join-Path $Data 'bad.yaml'
    Write-Utf8 $bad "id: bad`nname: Bad`nsteps:`n  - name: s`n    script: echo {{nope}}`n"
    Invoke-Psccm @('--data-dir', $Data, 'packages', 'validate', $bad) | Out-Null
    Assert ($script:ExitCode -ne 0) 'invalid package was accepted'
}

# ------------------------------------------------------------- start console

$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start(); $Port = $listener.LocalEndpoint.Port; $listener.Stop()
$Token = [Guid]::NewGuid().ToString('N')
$Base = "http://127.0.0.1:$Port"
$Proc = $null

Test-Step 'Console' 'Starts with a token' {
    $argList = @('--data-dir', "`"$Data`"", 'serve', '--no-browser', '--bind', '127.0.0.1', '--port', "$Port", '--token', $Token)
    $script:Proc = Start-Process -FilePath $Exe -ArgumentList $argList -PassThru -NoNewWindow `
        -RedirectStandardOutput (Join-Path $ReportDir 'console-out.log') -RedirectStandardError (Join-Path $ReportDir 'console-err.log')
    Assert (Wait-Url "$Base/" 30) 'console did not start'
}

try {
    # ------------------------------------------------------------------ API
    Test-Step 'API' 'Rejects requests without the token' {
        $r = Api GET '/summary' -NoAuth
        Assert ($r.Status -eq 401) "got HTTP $($r.Status)"
    }
    Test-Step 'API' 'Summary' {
        $s = Ok (Api GET '/summary')
        Assert ($s.packages -ge 17) "packages: $($s.packages)"
        Assert (@($s.package_errors).Count -eq 0) "package errors: $($s.package_errors)"
    }
    Test-Step 'API' 'Every built-in package round-trips and validates' {
        $list = (Ok (Api GET '/packages')).packages
        foreach ($p in $list) {
            $y = (Ok (Api GET "/packages/$($p.id)")).yaml
            $v = Ok (Api POST '/packages/validate' @{ yaml = $y })
            Assert $v.ok "$($p.id): $($v.error)"
        }
        "$(@($list).Count) packages"
    }
    Test-Step 'API' 'Package create / update / delete' {
        $y = "id: rt-crud`nname: CRUD test`nsteps:`n  - name: s`n    wait: 1`n"
        [void](Ok (Api POST '/packages' @{ yaml = $y }))
        Assert ((Api POST '/packages' @{ yaml = $y }).Status -eq 400) 'duplicate id accepted'
        [void](Ok (Api PUT '/packages/rt-crud' @{ yaml = $y.Replace('CRUD test', 'CRUD test 2') }))
        Assert ((Ok (Api GET '/packages/rt-crud')).package.name -eq 'CRUD test 2') 'update not saved'
        $bad = Ok (Api POST '/packages/validate' @{ yaml = "id: x`nname: X`nsteps: []`n" })
        Assert (-not $bad.ok) 'empty package validated'
        [void](Ok (Api DELETE '/packages/rt-crud'))
        Assert ((Api GET '/packages/rt-crud').Status -eq 404) 'not deleted'
    }
    Test-Step 'API' 'Computers add / edit / check / remove' {
        [void](Ok (Api POST '/computers' @{ names = "rt-a.invalid, rt-b.invalid"; os = 'windows'; tags = @('rt', 'rt-web') }))
        $c = @(Ok (Api GET '/computers')) | Where-Object { $_.name -like 'rt-*.invalid' }
        Assert (@($c).Count -eq 2) "found $(@($c).Count)"
        $a = $c | Where-Object { $_.name -eq 'rt-a.invalid' }
        [void](Ok (Api PUT "/computers/$($a.id)" @{ os = 'windows'; port = 5985; description = 'release test'; tags = @('rt', 'rt-web') }))
        $chk = Ok (Api POST '/computers/check' @{ names = @('rt-a.invalid', 'rt-b.invalid') })
        Assert ($chk.checked -eq 2 -and $chk.online -eq 0) "check: $($chk | ConvertTo-Json -Compress)"
        $a = @(Ok (Api GET '/computers')) | Where-Object { $_.name -eq 'rt-a.invalid' }
        Assert ($a.description -eq 'release test' -and $a.online -eq $false) 'edit/check not stored'
    }
    Test-Step 'API' 'Groups: static, dynamic, preview, members' {
        $s = Ok (Api POST '/groups' @{ name = 'RT static'; kind = 'static'; members = @('rt-a.invalid') })
        [void](Ok (Api POST "/groups/$($s.id)/members" @{ add = @('rt-b.invalid') }))
        [void](Ok (Api POST '/groups' @{ name = 'RT dynamic'; kind = 'dynamic'; rules = @{ name = 'rt-?.invalid'; tags = @('rt-web') } }))
        Assert ((Api POST '/groups' @{ name = 'RT empty'; kind = 'dynamic'; rules = @{} }).Status -eq 400) 'empty dynamic group accepted'
        $prev = Ok (Api POST '/groups/preview' @{ name = 'rt-a*' })
        Assert (@($prev.members).Count -eq 1) "preview: $($prev.members)"
        $g = (Ok (Api GET '/groups')).groups
        $st = $g | Where-Object { $_.name -eq 'RT static' }
        $dy = $g | Where-Object { $_.name -eq 'RT dynamic' }
        Assert (@($st.members).Count -eq 2) "static members: $($st.members)"
        Assert (@($dy.members).Count -eq 2) "dynamic members: $($dy.members)"
        [void](Ok (Api POST "/groups/$($s.id)/members" @{ remove = @('rt-b.invalid') }))
        $st = (Ok (Api GET '/groups')).groups | Where-Object { $_.name -eq 'RT static' }
        Assert (@($st.members).Count -eq 1) 'remove member failed'
    }
    Test-Step 'API' 'Inventory: settings, unreachable scan, rule validation' {
        [void](Ok (Api PUT '/settings/inventory' @{ interval_hours = 0; concurrency = 8; rescan_after_deploy = $true }))
        Assert ((Ok (Api GET '/inventory')).settings.concurrency -eq 8) 'inventory settings not saved'
        $inv = Invoke-Scan @('rt-a.invalid') 120
        Assert ($inv['rt-a.invalid'].status -eq 'offline') "status $($inv['rt-a.invalid'].status)"
        Assert (-not $inv['rt-a.invalid'].scanned_at) 'unreachable computer has a scan time'
        $bad = Api POST '/groups' @{ name = 'RT bad inv'; kind = 'dynamic'; rules = @{ app_version = '<130' } }
        Assert ($bad.Status -eq 400) 'version condition without software accepted'
        $prev = Ok (Api POST '/groups/preview' @{ app = '*' })
        Assert (@($prev.members).Count -eq 0) "unscanned computers matched a software rule: $($prev.members)"
        Assert ((Api GET '/inventory/computer/no-such-computer').Status -eq 404) 'missing inventory is not 404'
    }
    Test-Step 'API' 'Credentials never return secrets' {
        $c = Ok (Api POST '/credentials' @{ name = 'rt-cred'; kind = 'ssh'; username = 'u'; secret = 'p@ss-rt' })
        $raw = (Api GET '/credentials' -Raw).Body
        Assert ($raw -notlike '*p@ss-rt*') 'secret returned by the API'
        $cred = @(Ok (Api GET '/credentials')) | Where-Object { $_.name -eq 'rt-cred' }
        Assert $cred.has_secret 'has_secret false'
        [void](Ok (Api DELETE "/credentials/$($c.id)"))
    }
    Test-Step 'API' 'Schedules: weekly next run, past one-off rejected' {
        $r = Ok (Api POST '/schedules' @{ name = 'rt weekly'; package_id = 'reboot'; kind = 'weekly'; time = '03:00'; weekdays = @(5); targets = @{ groups = @('RT static') } })
        Assert ([datetime]$r.next_run -gt (Get-Date).ToUniversalTime()) "next_run $($r.next_run)"
        $bad = Api POST '/schedules' @{ name = 'rt past'; package_id = 'reboot'; kind = 'once'; time = '2001-01-01 00:00'; targets = @{ tags = @('rt') } }
        Assert ($bad.Status -eq 400) 'past schedule accepted'
        [void](Ok (Api DELETE "/schedules/$($r.id)"))
    }
    Test-Step 'API' 'Settings round-trip' {
        [void](Ok (Api PUT '/settings/winrm' @{ use_ssl = $false; skip_ca_check = $true }))
        $s = Ok (Api GET '/settings')
        Assert ($s.winrm.skip_ca_check -eq $true) 'winrm setting not saved'
        [void](Ok (Api PUT '/settings/winrm' @{ use_ssl = $false; skip_ca_check = $false }))
        Assert ($s.version) 'no version in settings'
    }
    Test-Step 'Deploy' 'Unreachable computer is reported offline' {
        $r = Invoke-Deploy @{ command = 'hostname'; targets = @{ computers = @('rt-a.invalid') } } 120
        Assert-Deploy $r 'failed' @('OFFLINE')
        Assert ($r.Targets[0].status -eq 'offline') "target: $($r.Targets[0].status)"
        $script:OfflineDeployment = $r.Id
    }
    Test-Step 'Deploy' 'Redeploy to failed computers' {
        $n = Ok (Api POST "/deployments/$script:OfflineDeployment/redeploy" @{ only_failed = $true })
        $r = Wait-Deployment $n.id 120
        Assert (@($r.Targets).Count -eq 1 -and $r.Targets[0].computer -eq 'rt-a.invalid') 'wrong targets'
        [void](Ok (Api DELETE "/deployments/$($n.id)"))
    }
    Test-Step 'Deploy' 'Deploy to a group' {
        $r = Invoke-Deploy @{ command = 'hostname'; targets = @{ groups = @('RT dynamic') } } 120
        Assert (@($r.Targets).Count -eq 2) "targets: $(@($r.Targets).Count)"
        $bad = Api POST '/deployments' @{ command = 'hostname'; targets = @{ groups = @('no such group') } }
        Assert ($bad.Status -ge 400) 'unknown group accepted'
    }
    Test-Step 'Deploy' 'Scheduled deployment can be cancelled' {
        $when = (Get-Date).AddHours(2).ToString('yyyy-MM-dd HH:mm')
        $r = Ok (Api POST '/deployments' @{ command = 'hostname'; targets = @{ tags = @('rt') }; scheduled_for = $when })
        $d = Ok (Api GET "/deployments/$($r.id)")
        Assert ($d.deployment.status -eq 'scheduled') "status $($d.deployment.status)"
        [void](Ok (Api POST "/deployments/$($r.id)/cancel"))
        Assert ((Ok (Api GET "/deployments/$($r.id)")).deployment.status -eq 'cancelled') 'not cancelled'
    }
    Test-Step 'Update' 'Status reports this version' {
        $u = Ok (Api GET '/update')
        Assert ($u.current -eq $script:Version) "current $($u.current) vs binary $script:Version"
    }
    Test-Step 'Update' 'Latest release is found and verifiable' {
        if (-not $UpdateRepo) { Skip 'pass -UpdateRepo owner/repo to test against published releases' }
        $u = Ok (Api GET '/update')
        $s = $u.settings
        $s.repo = $UpdateRepo
        [void](Ok (Api PUT '/settings/update' $s))
        $r = Ok (Api POST '/update/check')
        if ($ExpectedVersion) { Assert ($r.version -eq $ExpectedVersion) "latest release is $($r.version)" }
        Assert (-not $r.newer) 'this build thinks a newer version exists'
        Assert ($r.asset_name) 'no download for this platform in the release'
        Assert ($r.sha256) 'release has no SHA256SUMS entry for the download'
        "latest $($r.version): $($r.asset_name)"
    }

    # --------------------------------------------------------- GPO (offline)
    $GpoGuid = '{5C0E7A1D-7E57-4D1E-9A55-000000000001}'
    Test-Step 'GPO' 'Import a GPO backup folder' {
        $root = [IO.Path]::Combine($Data, 'gpo-src', $GpoGuid)
        $gpo = [IO.Path]::Combine($root, 'DomainSysvol', 'GPO')
        $secedit = [IO.Path]::Combine($gpo, 'Machine', 'Microsoft', 'Windows NT', 'SecEdit')
        $u = [Text.Encoding]::Unicode
        $pol = New-PolBytes @(
            @{ Key = 'SOFTWARE\Policies\PSCCM-ReleaseTest'; Name = 'Value1'; Type = 4; Data = [BitConverter]::GetBytes([uint32]42) },
            @{ Key = 'SOFTWARE\Policies\PSCCM-ReleaseTest'; Name = 'Text'; Type = 1; Data = $u.GetBytes("hello-gpo`0") },
            @{ Key = 'SOFTWARE\Policies\PSCCM-ReleaseTest'; Name = '**del.Old'; Type = 1; Data = $u.GetBytes(" `0") }
        )
        New-Item -ItemType Directory -Force -Path $secedit | Out-Null
        [IO.File]::WriteAllBytes([IO.Path]::Combine($gpo, 'Machine', 'Registry.pol'), $pol)
        $inf = "[Unicode]`r`nUnicode=yes`r`n[Version]`r`nsignature=`"`$CHICAGO`$`"`r`nRevision=1`r`n"
        [IO.File]::WriteAllBytes((Join-Path $secedit 'GptTmpl.inf'), [byte[]](@(0xFF, 0xFE) + $u.GetBytes($inf)))
        Write-Utf8 (Join-Path $gpo 'GPT.INI') "[General]`r`nVersion=65539`r`n"
        Write-Utf8 (Join-Path $root 'bkupInfo.xml') '<BackupInst><GPODisplayName><![CDATA[PSCCM Release Test GPO]]></GPODisplayName></BackupInst>'
        $m = Ok (Api POST '/gpos/import-folder' @{ path = $root })
        Assert ($m.name -eq 'PSCCM Release Test GPO') "name '$($m.name)'"
        Assert ($m.machine_settings -eq 3) "settings $($m.machine_settings)"
        Assert ($m.has_security) 'security template not detected'
        Assert ($m.computer_version -eq 3) "version $($m.computer_version)"
        $script:GpoPackage = $m.package_id
        $snap = Ok (Api GET "/gpos/$($m.guid)")
        Assert (@($snap.snapshot.machine | Where-Object { $_.n -eq 'Value1' -and $_.v -eq 42 }).Count -eq 1) 'Value1 not parsed'
        Assert ((Ok (Api GET "/packages/$($m.package_id)")).package.steps.Count -ge 3) 'generated package incomplete'
        $m.package_id
    }

    # ---------------------------------------------------------- repository
    $repo = Join-Path $Data 'repository'
    Write-Utf8 (Join-Path $repo 'rt/hello.txt') 'hello from repository'
    Write-Utf8 (Join-Path $repo 'rt/fake-setup.cmd') "@echo off`r`necho fake installer args: %*`r`nexit /b 0`r`n"
    Write-Utf8 (Join-Path $repo 'rt/fake-setup.sh') "echo `"fake installer args: `$*`"`n"
    $shaCmd = (Get-FileHash -Algorithm SHA256 (Join-Path $repo 'rt/fake-setup.cmd')).Hash.ToLower()

    # --------------------------------------------------------- Windows target
    if ($WindowsTarget) {
        Test-Step 'Windows' 'Register target' {
            if ($WindowsUser) {
                [void](Ok (Api POST '/credentials' @{ name = 'rt-win'; kind = 'windows'; username = $WindowsUser; secret = $WindowsPassword; default_for = 'windows' }))
            }
            [void](Ok (Api POST '/computers' @{ names = $WindowsTarget; os = 'windows'; tags = @('rt-win') }))
            $chk = Ok (Api POST '/computers/check' @{ names = @($WindowsTarget) })
            Assert ($chk.online -eq 1) 'WinRM port not reachable'
        }
        Test-Step 'Windows' 'Ad-hoc PowerShell over WinRM' {
            $r = Invoke-Deploy @{ command = "Write-Output ('ps=' + `$PSVersionTable.PSVersion.Major); hostname"; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $r 'success' @('ps=')
        }
        Test-Step 'Windows' 'Inventory scan over WinRM and a software group rule' {
            $i = (Invoke-Scan @($WindowsTarget))[$WindowsTarget]
            Assert ($i.status -eq 'ok') "scan $($i.status): $($i.error)"
            Assert ($i.facts.os_name -like '*Windows*') "os_name '$($i.facts.os_name)'"
            Assert ([int]$i.facts.ram_mb -gt 0) 'no memory size'
            Assert (@($i.disks).Count -ge 1) 'no disks'
            Assert ($i.app_count -gt 0) 'no installed programs'
            $full = Ok (Api GET "/inventory/computer/$WindowsTarget")
            $app = @($full.apps)[0].name
            $prev = Ok (Api POST '/groups/preview' @{ app = $app })
            Assert (@($prev.members) -contains $WindowsTarget) "software rule '$app' did not match"
            $prev = Ok (Api POST '/groups/preview' @{ app = $app; app_missing = $true })
            Assert (@($prev.members) -notcontains $WindowsTarget) "'without $app' matched"
            "$($i.facts.os_name), $($i.app_count) programs"
        }
        Test-Step 'Windows' 'cmd shell' {
            $r = Invoke-Deploy @{ command = 'echo cmd-ok %COMPUTERNAME%'; shell = 'cmd'; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $r 'success' @('cmd-ok')
        }
        Test-Step 'Windows' 'Every step type (script, exit codes, cmd, when, retry, copy, install, service, include, delegate)' {
            [void](Add-Package "id: rt-child-win`nname: RT child`nplatforms: [windows]`nsteps:`n  - name: Child step`n    script: Write-Output child-ok`n")
            $y = @"
id: rt-selftest-win
name: Release test (Windows)
platforms: [windows]
variables:
  greeting: { default: "héllo" }
  pw: { default: "s3cr3t-rt-value", secret: true }
steps:
  - name: Variables and secret masking
    script: Write-Output "{{greeting}} from {{host}} pw={{pw}}"
  - name: Native exit code
    script: cmd.exe /c exit 3010
    success_codes: [3010]
  - name: cmd shell
    shell: cmd
    script: echo cmd-step-ok
  - name: Condition false skips
    when: exit 1
    script: throw "must not run"
  - name: Retry then continue
    retries: 1
    retry_delay: 1
    continue_on_error: true
    script: throw "deliberate"
  - name: Copy from repository
    copy: { src: rt/hello.txt, dest: '{{workdir}}\out\hello.txt' }
  - name: Read copy
    script: Get-Content '{{workdir}}\out\hello.txt'
  - name: Install from repository
    install: { file: rt/fake-setup.cmd, args: "/S /D=C:\\X", sha256: $shaCmd }
  - name: Service action
    service: { name: WinRM, action: start }
  - name: Wait
    wait: 1
  - name: Nested package
    include: rt-child-win
  - name: Delegated step
    delegate_to: "{{host}}"
    script: Write-Output delegated-ok
verify:
  windows: exit 0
"@
            [void](Add-Package $y)
            $r = Invoke-Deploy @{ package_id = 'rt-selftest-win'; targets = @{ computers = @($WindowsTarget) } } 600
            Assert-Deploy $r 'success' @('héllo from', 'pw=********', 'reboot is required', 'cmd-step-ok', 'condition not met', 'retry 1/1', 'continue_on_error', 'hello from repository', 'fake installer args: /S', 'Installer exit code: 0', 'WinRM is Running', 'waiting 1s', 'child-ok', 'delegated-ok', '==> Verify') @('s3cr3t-rt-value', 'must not run')
        }
        Test-Step 'Windows' 'Failure runs rollback' {
            [void](Add-Package "id: rt-fail-win`nname: RT fail`nplatforms: [windows]`nsteps:`n  - name: One`n    script: Write-Output step-one`n  - name: Two fails`n    script: exit 7`nrollback:`n  - name: Undo`n    script: Write-Output rolled-back-ok`n")
            $r = Invoke-Deploy @{ package_id = 'rt-fail-win'; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $r 'failed' @('step-one', 'exit code 7', 'Rolling back', 'rolled-back-ok')
        }
        Test-Step 'Windows' 'Detect skips computers that already have it' {
            [void](Add-Package "id: rt-detect-win`nname: RT detect`nplatforms: [windows]`ndetect:`n  windows: exit 0`nsteps:`n  - name: Must not run`n    script: throw 'ran'`n")
            $r = Invoke-Deploy @{ package_id = 'rt-detect-win'; targets = @{ computers = @($WindowsTarget) } } 300
            Assert ($r.Targets[0].status -eq 'skipped') "target status $($r.Targets[0].status)"
        }
        Test-Step 'Windows' 'Rolling batch stops after a failure' {
            $r = Invoke-Deploy @{ command = 'hostname'; targets = @{ computers = @('rt-b.invalid', $WindowsTarget) }; options = @{ batch_size = 1; max_failures = 0; concurrency = 1 } } 300
            $t = $r.Targets | Where-Object { $_.computer -eq $WindowsTarget }
            Assert ($t.status -eq 'cancelled') "second computer was $($t.status), expected cancelled"
        }
        Test-Step 'Windows' 'CLI deploy (exit code 0)' {
            $out = Invoke-Psccm @('--data-dir', $Data, 'deploy', 'rt-child-win', '-t', $WindowsTarget) | Out-String
            Assert ($script:ExitCode -eq 0) "exit $script:ExitCode`n$out"
            Assert ($out -like '*child-ok*') $out
        }
        Test-Step 'Windows' 'GPO apply and remove (registry + secedit)' {
            if (-not $RealInstalls) { Skip 'changes HKLM policies on the target; run with -RealInstalls' }
            if (-not $script:GpoPackage) { Skip 'GPO import failed' }
            $r = Invoke-Deploy @{ package_id = $script:GpoPackage; targets = @{ computers = @($WindowsTarget) } } 600
            Assert-Deploy $r 'success' @('Registry policies: 2 set', 'Security policy')
            $chk = Invoke-Deploy @{ command = "(Get-ItemProperty 'HKLM:\SOFTWARE\Policies\PSCCM-ReleaseTest').Value1"; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $chk 'success' @('42')
            $r = Invoke-Deploy @{ package_id = $script:GpoPackage; variables = @{ mode = 'remove' }; targets = @{ computers = @($WindowsTarget) } } 600
            Assert-Deploy $r 'success' @('2 removed')
            $chk = Invoke-Deploy @{ command = "if ((Get-Item 'HKLM:\SOFTWARE\Policies\PSCCM-ReleaseTest').GetValueNames().Count) { exit 1 }; Remove-Item 'HKLM:\SOFTWARE\Policies\PSCCM-ReleaseTest'; 'clean'"; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $chk 'success' @('clean')
        }
        Test-Step 'Windows' '7-Zip install, then detect skips' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            $r = Invoke-Deploy @{ package_id = '7zip'; options = @{ use_detect = $false }; targets = @{ computers = @($WindowsTarget) } } 900
            Assert-Deploy $r 'success' @('==> Verify')
            $r = Invoke-Deploy @{ package_id = '7zip'; targets = @{ computers = @($WindowsTarget) } } 600
            Assert ($r.Targets[0].status -eq 'skipped') "second run: $($r.Targets[0].status)"
        }
        Test-Step 'Windows' 'Uninstall, Node.js MSI install, same version is not reinstalled' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            # Start clean (runner images ship Node.js): this also exercises uninstall-app.
            $r = Invoke-Deploy @{ package_id = 'uninstall-app'; variables = @{ name = 'Node.js' }; targets = @{ computers = @($WindowsTarget) } } 900
            if ($r.Targets[0].status -ne 'skipped') { Assert-Deploy $r 'success' @('Uninstalling Node.js') }
            $r = Invoke-Deploy @{ package_id = 'nodejs'; variables = @{ version = '22' }; targets = @{ computers = @($WindowsTarget) } } 1200
            Assert-Deploy $r 'success' @('Running: msiexec.exe', '==> Verify', 'v22.')
            $r = Invoke-Deploy @{ package_id = 'nodejs'; variables = @{ version = '22' }; options = @{ use_detect = $false }; targets = @{ computers = @($WindowsTarget) } } 600
            Assert-Deploy $r 'success' @('is already installed') @('Running: msiexec.exe')
        }
        Test-Step 'Windows' 'IIS install, site deploy with health check, app pool recycle' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            $site = Join-Path $Data 'site'
            $marker = 'psccm-site-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
            Write-Utf8 (Join-Path $site 'index.html') "<html><body>$marker</body></html>"
            New-Item -ItemType Directory -Force -Path (Join-Path $repo 'web') | Out-Null
            Compress-Archive -Path (Join-Path $site '*') -DestinationPath (Join-Path $repo 'web/site.zip') -Force
            $r = Invoke-Deploy @{ package_id = 'iis-install'; variables = @{ features = 'Web-Server,Web-Mgmt-Console' }; targets = @{ computers = @($WindowsTarget) } } 2400
            Assert-Deploy $r 'success'
            $r = Invoke-Deploy @{ package_id = 'iis-site-deploy'; variables = @{ site = 'Default Web Site'; app_pool = 'DefaultAppPool'; package = 'web/site.zip'; health_url = 'http://localhost/' }; targets = @{ computers = @($WindowsTarget) } } 900
            Assert-Deploy $r 'success' @('Extracted to', 'http://localhost/ -> 200')
            $chk = Invoke-Deploy @{ command = "(Invoke-WebRequest http://localhost/ -UseBasicParsing).Content"; targets = @{ computers = @($WindowsTarget) } } 300
            Assert-Deploy $chk 'success' @($marker)
            $r = Invoke-Deploy @{ package_id = 'iis-apppool-recycle'; variables = @{ pools = 'DefaultAppPool' }; targets = @{ computers = @($WindowsTarget) } } 600
            Assert-Deploy $r 'success' @('recycled DefaultAppPool')
        }
    } else {
        Test-Step 'Windows' 'Target tests' { Skip 'pass -WindowsTarget to test WinRM deployments' }
    }

    # ----------------------------------------------------------- Linux target
    if ($LinuxTarget) {
        Test-Step 'Linux' 'Register target' {
            Assert ($LinuxUser) '-LinuxUser is required'
            $body = @{ name = 'rt-linux'; kind = 'ssh'; username = $LinuxUser; default_for = 'linux' }
            if ($LinuxPassword) { $body.secret = $LinuxPassword }
            if ($LinuxKey) { $body.key_path = (Resolve-Path $LinuxKey).Path }
            [void](Ok (Api POST '/credentials' $body))
            [void](Ok (Api POST '/computers' @{ names = $LinuxTarget; os = 'linux'; tags = @('rt-linux') }))
            $chk = Ok (Api POST '/computers/check' @{ names = @($LinuxTarget) })
            Assert ($chk.online -eq 1) 'SSH port not reachable'
        }
        Test-Step 'Linux' 'Ad-hoc command over SSH with sudo' {
            $r = Invoke-Deploy @{ command = 'echo "kernel=$(uname -s) uid=$(id -u)"'; targets = @{ computers = @($LinuxTarget) } } 300
            Assert-Deploy $r 'success' @('kernel=Linux uid=0')
        }
        Test-Step 'Linux' 'Inventory scan over SSH' {
            $i = (Invoke-Scan @($LinuxTarget))[$LinuxTarget]
            Assert ($i.status -eq 'ok') "scan $($i.status): $($i.error)"
            Assert ($i.facts.os_name) 'no os_name'
            Assert ($i.facts.kernel) 'no kernel'
            Assert (@($i.disks).Count -ge 1) 'no disks'
            Assert ($i.app_count -gt 0) 'no installed packages'
            $prev = Ok (Api POST '/groups/preview' @{ app = 'bash' })
            Assert (@($prev.members) -contains $LinuxTarget) 'software rule "bash" did not match'
            "$($i.facts.os_name), $($i.app_count) packages"
        }
        Test-Step 'Linux' 'Every step type (script, when, retry, copy, install, service, include, delegate)' {
            [void](Add-Package "id: rt-child-linux`nname: RT child`nplatforms: [linux]`nsteps:`n  - name: Child step`n    script: echo child-ok`n")
            $y = @"
id: rt-selftest-linux
name: Release test (Linux)
platforms: [linux]
variables:
  greeting: { default: "héllo" }
  pw: { default: "s3cr3t-rt-value", secret: true }
steps:
  - name: Variables and secret masking
    script: echo "{{greeting}} from {{host}} pw={{pw}}"
  - name: Exit code mapping
    script: exit 3
    success_codes: [3]
  - name: sh shell
    shell: sh
    script: echo sh-step-ok
  - name: Condition false skips
    when: exit 1
    script: echo must-not-run
  - name: Retry then continue
    retries: 1
    retry_delay: 1
    continue_on_error: true
    script: exit 5
  - name: Copy from repository
    copy: { src: rt/hello.txt, dest: /tmp/psccm-rt/hello.txt }
  - name: Read copy
    script: cat /tmp/psccm-rt/hello.txt && rm -rf /tmp/psccm-rt
  - name: Install from repository
    install: { file: rt/fake-setup.sh, args: "--quiet" }
  - name: Service action
    service: { name: ssh, action: restart }
  - name: Nested package
    include: rt-child-linux
  - name: Delegated step
    delegate_to: "{{host}}"
    script: echo delegated-ok
verify:
  linux: test "`$(id -u)" = 0
"@
            [void](Add-Package $y)
            $r = Invoke-Deploy @{ package_id = 'rt-selftest-linux'; targets = @{ computers = @($LinuxTarget) } } 600
            Assert-Deploy $r 'success' @('héllo from', 'pw=********', 'sh-step-ok', 'condition not met', 'retry 1/1', 'hello from repository', 'fake installer args: --quiet', 'child-ok', 'delegated-ok', '==> Verify') @('s3cr3t-rt-value', 'must-not-run')
        }
        Test-Step 'Linux' 'Failure runs rollback' {
            [void](Add-Package "id: rt-fail-linux`nname: RT fail`nplatforms: [linux]`nsteps:`n  - name: One`n    script: echo step-one`n  - name: Two fails`n    script: exit 7`nrollback:`n  - name: Undo`n    script: echo rolled-back-ok`n")
            $r = Invoke-Deploy @{ package_id = 'rt-fail-linux'; targets = @{ computers = @($LinuxTarget) } } 300
            Assert-Deploy $r 'failed' @('step-one', 'exit code 7', 'Rolling back', 'rolled-back-ok')
        }
        Test-Step 'Linux' 'Detect skips computers that already have it' {
            [void](Add-Package "id: rt-detect-linux`nname: RT detect`nplatforms: [linux]`ndetect:`n  linux: exit 0`nsteps:`n  - name: Must not run`n    script: exit 1`n")
            $r = Invoke-Deploy @{ package_id = 'rt-detect-linux'; targets = @{ computers = @($LinuxTarget) } } 300
            Assert ($r.Targets[0].status -eq 'skipped') "target status $($r.Targets[0].status)"
        }
        Test-Step 'Linux' 'CLI run (exit code 0)' {
            $out = Invoke-Psccm @('--data-dir', $Data, 'run', $LinuxTarget, '-s', 'echo cli-ok') | Out-String
            Assert ($script:ExitCode -eq 0) "exit $script:ExitCode`n$out"
            Assert ($out -like '*cli-ok*') $out
        }
        Test-Step 'Linux' '7-Zip install, then detect skips' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            $r = Invoke-Deploy @{ package_id = '7zip'; options = @{ use_detect = $false }; targets = @{ computers = @($LinuxTarget) } } 900
            Assert-Deploy $r 'success' @('==> Verify')
            $r = Invoke-Deploy @{ package_id = '7zip'; targets = @{ computers = @($LinuxTarget) } } 600
            Assert ($r.Targets[0].status -eq 'skipped') "second run: $($r.Targets[0].status)"
        }
        Test-Step 'Linux' 'Node.js 22 install' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            $r = Invoke-Deploy @{ package_id = 'nodejs'; variables = @{ version = '22' }; options = @{ use_detect = $false }; targets = @{ computers = @($LinuxTarget) } } 1200
            Assert-Deploy $r 'success' @('v22.')
        }
        Test-Step 'Linux' 'HAProxy install/upgrade, config deploy, drain and ready' {
            if (-not $RealInstalls) { Skip 'run with -RealInstalls' }
            $cfg = @"
global
    log /dev/log local0
    stats socket /run/haproxy/admin.sock mode 660 level admin
    daemon
defaults
    mode http
    timeout connect 5s
    timeout client 30s
    timeout server 30s
frontend fe
    bind 127.0.0.1:8081
    default_backend web
backend web
    server s1 127.0.0.1:9 check
"@
            # Deliberately no final newline: the package must cope with that.
            Write-Utf8 (Join-Path $repo 'haproxy/haproxy.cfg') $cfg.Replace("`r`n", "`n").TrimEnd("`n")
            $r = Invoke-Deploy @{ package_id = 'haproxy-upgrade'; targets = @{ computers = @($LinuxTarget) } } 1800
            Assert-Deploy $r 'success' @('HAProxy version', 'Configuration file is valid')
            $r = Invoke-Deploy @{ package_id = 'haproxy-config'; targets = @{ computers = @($LinuxTarget) } } 600
            Assert-Deploy $r 'success' @('Configuration file is valid', 'backup:')
            $r = Invoke-Deploy @{ package_id = 'haproxy-server-state'; variables = @{ backend = 'web'; server = 's1'; state = 'drain'; wait_seconds = '5' }; targets = @{ computers = @($LinuxTarget) } } 600
            Assert-Deploy $r 'success' @('web/s1 -> drain')
            $r = Invoke-Deploy @{ package_id = 'haproxy-server-state'; variables = @{ backend = 'web'; server = 's1'; state = 'ready' }; targets = @{ computers = @($LinuxTarget) } } 600
            Assert-Deploy $r 'success' @('web/s1 -> ready')
            Write-Utf8 (Join-Path $repo 'haproxy/haproxy.cfg') "this is not a valid config`n"
            $r = Invoke-Deploy @{ package_id = 'haproxy-config'; targets = @{ computers = @($LinuxTarget) } } 600
            Assert-Deploy $r 'failed' @('Rolling back')
            $chk = Invoke-Deploy @{ command = 'systemctl is-active haproxy && haproxy -c -f /etc/haproxy/haproxy.cfg'; targets = @{ computers = @($LinuxTarget) } } 300
            Assert-Deploy $chk 'success' @('active')
        }
    } else {
        Test-Step 'Linux' 'Target tests' { Skip 'pass -LinuxTarget/-LinuxUser to test SSH deployments' }
    }

    if ($AdTests) {
        Test-Step 'AD' 'Computers can be read from AD' {
            $r = Ok (Api POST '/ad/preview' @{})
            Assert (@($r.computers).Count -gt 0) 'no computers returned'
            "$(@($r.computers).Count) computers"
        }
        Test-Step 'AD' 'GPOs can be listed' {
            $g = (Ok (Api GET '/gpos')).gpos
            Assert (@($g).Count -gt 0) 'no GPOs returned'
            "$(@($g).Count) GPOs"
        }
    }

    Test-Step 'Console' 'History via CLI' {
        $out = Invoke-Psccm @('--data-dir', $Data, 'history') | Out-String
        Assert ($script:ExitCode -eq 0) $out
        Assert ($out -match 'Command: hostname') 'history does not show the deployments'
    }
} finally {
    if ($Proc -and -not $Proc.HasExited) { Stop-Process -Id $Proc.Id -Force -ErrorAction SilentlyContinue }
}

if ($Setup -and $OnWindows) {
    Test-Step 'Installer' 'Silent uninstall removes the service' {
        $un = Join-Path $env:ProgramFiles 'PSCCM\unins000.exe'
        $p = Start-Process -FilePath $un -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -PassThru
        # The uninstaller hands off to a copy of itself in %TEMP%; wait for that too.
        $deadline = (Get-Date).AddMinutes(5)
        while ((-not $p.HasExited -or (Get-Process _unins* -ErrorAction SilentlyContinue)) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
        $left = Get-Process _unins* -ErrorAction SilentlyContinue
        if ($left) { $left | Stop-Process -Force; throw 'uninstaller still running after 5 minutes (waiting on a prompt?)' }
        Assert ($p.ExitCode -eq 0) "uninstaller exit $($p.ExitCode)"
        Start-Sleep -Seconds 3
        Assert (-not (Get-Service PSCCM -ErrorAction SilentlyContinue)) 'service still exists'
        Assert (-not (Test-Path $InstalledExe)) 'psccm.exe still installed'
        Assert (Test-Path (Join-Path $env:ProgramData 'PSCCM\psccm.db')) 'silent uninstall must keep the data folder'
    }
}

Start-Sleep -Milliseconds 500
Remove-Item -Recurse -Force $Data -ErrorAction SilentlyContinue

# ------------------------------------------------------------------- report

$Finished = Get-Date
$pass = @($Results | Where-Object status -eq 'pass').Count
$fail = @($Results | Where-Object status -eq 'fail').Count
$skip = @($Results | Where-Object status -eq 'skip').Count
$os = if ($OnWindows) { [Environment]::OSVersion.VersionString } else { (uname -sr) }
$report = [ordered]@{
    product = 'PSCCM'; version = $Version; expected_version = $ExpectedVersion; executable = $Exe
    started = $Started.ToUniversalTime().ToString('o'); finished = $Finished.ToUniversalTime().ToString('o')
    duration_seconds = [int]($Finished - $Started).TotalSeconds
    machine = [Environment]::MachineName; os = $os; powershell = $PSVersionTable.PSVersion.ToString()
    targets = [ordered]@{ windows = $WindowsTarget; linux = $LinuxTarget; real_installs = [bool]$RealInstalls; installer = [bool]$Setup }
    totals = [ordered]@{ pass = $pass; fail = $fail; skip = $skip; total = $Results.Count }
    results = $Results
}
Write-Utf8 (Join-Path $ReportDir 'report.json') ($report | ConvertTo-Json -Depth 6)

$enc = { param($s) [Net.WebUtility]::HtmlEncode([string]$s) }
$rows = ($Results | Group-Object group | ForEach-Object {
    $g = $_
    "<tr class='grp'><th colspan='4'>$(& $enc $g.Name)</th></tr>" + (($g.Group | ForEach-Object {
        $d = if ($_.detail) { "<pre>$(& $enc $_.detail)</pre>" } else { '' }
        "<tr><td><span class='pill $($_.status)'>$($_.status)</span></td><td>$(& $enc $_.name)$d</td><td class='num'>$([math]::Round($_.ms / 1000, 1)) s</td></tr>"
    }) -join "`n")
}) -join "`n"
$verdict = if ($fail) { "<span class='pill fail'>FAILED</span>" } else { "<span class='pill pass'>PASSED</span>" }
$html = @"
<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>PSCCM Test Report</title>
<style>
:root{--bg:#f5f7fa;--panel:#fff;--text:#1c2330;--muted:#667085;--border:#e3e7ee;--ok:#15803d;--okb:#e3f6e9;--bad:#c62828;--badb:#fde8e8;--skip:#92400e;--skipb:#fdf1df;--code:#0f172a;--codet:#dbe4f0}
@media (prefers-color-scheme:dark){:root:not([data-theme=light]){--bg:#0d1117;--panel:#151b24;--text:#e3e8ef;--muted:#8b96a8;--border:#273141;--ok:#4ade80;--okb:#13301f;--bad:#f87171;--badb:#3a1717;--skip:#fbbf24;--skipb:#3a2a0c;--code:#0a0e14}}
body{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 system-ui,Segoe UI,sans-serif}
main{max-width:1100px;margin:0 auto;padding:24px 16px}
h1{font-size:22px;margin:0 0 4px}.muted{color:var(--muted)}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:12px;margin:18px 0}
.stat{background:var(--panel);border:1px solid var(--border);border-radius:10px;padding:12px 16px}.stat b{display:block;font-size:26px}
table{width:100%;border-collapse:collapse;background:var(--panel);border:1px solid var(--border);border-radius:10px;overflow:hidden}
td,th{padding:8px 12px;border-bottom:1px solid var(--border);text-align:left;vertical-align:top}
tr.grp th{background:var(--bg);font-size:12px;text-transform:uppercase;letter-spacing:.05em;color:var(--muted)}
.num{white-space:nowrap;color:var(--muted);text-align:right}
.pill{display:inline-block;padding:1px 9px;border-radius:10px;font-size:12px;font-weight:600}
.pass{background:var(--okb);color:var(--ok)}.fail{background:var(--badb);color:var(--bad)}.skip{background:var(--skipb);color:var(--skip)}
pre{background:var(--code);color:var(--codet);padding:8px 10px;border-radius:6px;white-space:pre-wrap;word-break:break-word;font-size:12px;margin:6px 0 0;max-height:320px;overflow:auto}
dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 14px;font-size:13px}dt{color:var(--muted)}dd{margin:0;word-break:break-all}
</style></head><body><main>
<h1>PSCCM $(& $enc $Version) release test $verdict</h1>
<div class="muted">$(& $enc $Started.ToString('yyyy-MM-dd HH:mm')) · $($report.duration_seconds) s · $(& $enc $report.machine)</div>
<div class="stats"><div class="stat"><b style="color:var(--ok)">$pass</b>passed</div><div class="stat"><b style="color:var(--bad)">$fail</b>failed</div>
<div class="stat"><b style="color:var(--skip)">$skip</b>skipped</div><div class="stat"><b>$($Results.Count)</b>total</div></div>
<dl><dt>Executable</dt><dd>$(& $enc $Exe)</dd><dt>OS</dt><dd>$(& $enc $os)</dd><dt>PowerShell</dt><dd>$(& $enc $report.powershell)</dd>
<dt>Windows target</dt><dd>$(& $enc $(if ($WindowsTarget) { $WindowsTarget } else { '-' }))</dd><dt>Linux target</dt><dd>$(& $enc $(if ($LinuxTarget) { $LinuxTarget } else { '-' }))</dd>
<dt>Real installs</dt><dd>$([bool]$RealInstalls)</dd><dt>Installer tested</dt><dd>$([bool]$Setup)</dd></dl>
<br><table><tbody>
$rows
</tbody></table></main></body></html>
"@
Write-Utf8 (Join-Path $ReportDir 'report.html') $html

$cases = ($Results | ForEach-Object {
    $n = [Security.SecurityElement]::Escape($_.name); $c = [Security.SecurityElement]::Escape($_.group)
    $inner = switch ($_.status) {
        'fail' { "<failure message=`"$([Security.SecurityElement]::Escape(($_.detail -split "`n")[0]))`">$([Security.SecurityElement]::Escape($_.detail))</failure>" }
        'skip' { "<skipped message=`"$([Security.SecurityElement]::Escape($_.detail))`"/>" }
        default { '' }
    }
    "  <testcase classname=`"$c`" name=`"$n`" time=`"$([math]::Round($_.ms / 1000, 3))`">$inner</testcase>"
}) -join "`n"
Write-Utf8 (Join-Path $ReportDir 'junit.xml') "<?xml version=`"1.0`" encoding=`"UTF-8`"?>`n<testsuite name=`"psccm-release`" tests=`"$($Results.Count)`" failures=`"$fail`" skipped=`"$skip`">`n$cases`n</testsuite>`n"

Write-Host ''
Write-Host ("{0} passed, {1} failed, {2} skipped - report: {3}" -f $pass, $fail, $skip, (Join-Path $ReportDir 'report.html')) -ForegroundColor $(if ($fail) { 'Red' } else { 'Green' })
if ($fail) { exit 1 }
exit 0
