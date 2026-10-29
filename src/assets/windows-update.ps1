# PSCCM Windows Update: scan for or install missing updates with the Windows Update Agent.
# Runs as SYSTEM (see wu-runner.ps1). The console puts these variables in front of it:
#   $WuMode     'scan' or 'install'
#   $WuSource   'default' (WSUS when one is configured, else Windows Update) or 'windows-update'
#   $WuClasses  classifications to install, '*' = all          (install)
#   $WuKbs      install only these KBs / update IDs (empty = all) (install)
#   $WuExclude  never install these KBs / update IDs             (install)
#   $WuExitFile where to write the exit code (set when run as a scheduled task)
# Data lines are "@@wu<TAB>kind<TAB>fields..."; non-ASCII characters and backslashes are
# written as \uXXXX, like the inventory scan.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false } catch {}

function Out-Wu {
    param([string]$Kind, [object[]]$Fields)
    $clean = foreach ($f in $Fields) {
        $s = ("$f" -replace '[\t\r\n]+', ' ').Trim()
        [regex]::Replace($s, '[^\x20-\x5b\x5d-\x7e]', { param($m) '\u{0:x4}' -f [int][char]$m.Value })
    }
    Write-Output ("@@wu`t$Kind`t" + ($clean -join "`t"))
}

# Classification IDs are the same everywhere; their names are localized.
$ClassNames = @{
    '0fa1201d-4330-4fa8-8ae9-b877473b6441' = 'Security Updates'
    'e6cf1350-c01b-414d-a61f-263d14d133b4' = 'Critical Updates'
    'e0789628-ce08-4437-be74-2495b842f43b' = 'Definition Updates'
    '28bc880e-0592-4cbf-8f95-c79b17911d5f' = 'Update Rollups'
    'cd5ffd1e-e932-4e3a-bf74-18bf0b1bbd83' = 'Updates'
    'b54e7d24-7add-428f-8b75-90a396fa584f' = 'Feature Packs'
    '68c5b0a3-d1a6-4553-ae49-01d3a7827828' = 'Service Packs'
    'b4832bd8-e735-4761-8daf-37f882276dab' = 'Tools'
    'ebfc1fc5-71a4-4f7b-9aca-3b9a503104a0' = 'Drivers'
    '3689bdc8-b205-4af4-8d4a-a63924c5e9d5' = 'Upgrades'
}

function Get-WuClass($u) {
    foreach ($c in $u.Categories) {
        if ($c.Type -eq 'UpdateClassification') {
            $n = $ClassNames["$($c.CategoryID)"]
            if ($n) { return $n }
            return "$($c.Name)"
        }
    }
    if ($u.Type -eq 2) { return 'Drivers' }
    return 'Updates'
}

function Get-WuKb($u) {
    foreach ($k in $u.KBArticleIDs) { return "KB$k" }
    return ''
}

function Format-Hresult([int]$h) { '0x{0:X8}' -f $h }

# A readable reason for the errors people actually hit.
function Get-WuHint([int]$h) {
    switch (Format-Hresult $h) {
        { $_ -in '0x8024402C', '0x80072EFD', '0x80072EE7', '0x80072EFE', '0x8024401B', '0x80244019' } { return 'cannot reach the update server (Windows Update or WSUS): check the network, proxy and WSUS address' }
        { $_ -in '0x80072EE2', '0x80244022', '0x8024401C', '0x8024401F', '0x80244023' } { return 'the update server timed out or is busy' }
        '0x8024002E' { return 'Windows Update access is turned off by policy (DisableWindowsUpdateAccess)' }
        '0x80070422' { return 'the Windows Update service (wuauserv) is disabled' }
        '0x80240438' { return 'there is no route to the update server' }
        '0x8024000B' { return 'the operation was cancelled' }
        '0x80070070' { return 'not enough disk space' }
        '0x8024200D' { return 'the update needs to be downloaded again' }
        '0x80242016' { return 'the update needs a restart before it can finish' }
    }
    return ''
}

function Format-WuError($err) {
    # PowerShell wraps the COM error; the HRESULT is on the innermost exception.
    $ex = $err.Exception
    while ($ex.InnerException) { $ex = $ex.InnerException }
    $h = $ex.HResult
    $hint = Get-WuHint $h
    $msg = "$($err.Exception.Message)".Trim()
    if ($hint) { return "$hint ($(Format-Hresult $h))" }
    return $msg
}

$ResultNames = @{ 0 = 'not started'; 1 = 'in progress'; 2 = 'succeeded'; 3 = 'succeeded with errors'; 4 = 'failed'; 5 = 'aborted' }

function Test-RebootPending {
    $si = try { (New-Object -ComObject Microsoft.Update.SystemInfo).RebootRequired } catch { $false }
    $cbs = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending'
    $wu = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired'
    return [bool]($si -or $cbs -or $wu)
}

function Search-Wu {
    $script:Session = New-Object -ComObject Microsoft.Update.Session
    $script:Session.ClientApplicationID = 'PSCCM'
    $searcher = $script:Session.CreateUpdateSearcher()
    # 2 = Windows Update itself, ignoring a configured WSUS server.
    if ($WuSource -eq 'windows-update') { $searcher.ServerSelection = 2 }
    try {
        $r = $searcher.Search('IsInstalled=0 and IsHidden=0')
    } catch {
        throw "searching for updates failed: $(Format-WuError $_)"
    }
    if ($r.ResultCode -notin 2, 3) { throw "searching for updates $($ResultNames[[int]$r.ResultCode])" }
    return @{ Searcher = $searcher; Updates = @($r.Updates | ForEach-Object { $_ }) }
}

function Get-WuSourceName {
    if ($WuSource -eq 'windows-update') { return 'Windows Update' }
    $au = Get-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -ErrorAction SilentlyContinue
    $wsus = (Get-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate' -ErrorAction SilentlyContinue).WUServer
    if ($au -and $au.UseWUServer -eq 1 -and $wsus) { return "WSUS $wsus" }
    return 'Windows Update'
}

function Get-IsoTime($d) {
    if ($null -eq $d) { return '' }
    try {
        $t = [datetime]$d
        if ($t.Year -lt 2000) { return '' }
        return $t.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    } catch { return '' }
}

function Invoke-WuScan {
    $found = Search-Wu
    Out-Wu info @('source', (Get-WuSourceName))
    Out-Wu info @('reboot_pending', $(if (Test-RebootPending) { 'true' } else { 'false' }))
    try {
        $res = (New-Object -ComObject Microsoft.Update.AutoUpdate).Results
        Out-Wu info @('last_install', (Get-IsoTime $res.LastInstallationSuccessDate))
    } catch {}
    try {
        $os = Get-CimInstance Win32_OperatingSystem
        $ubr = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction Stop).UBR
        Out-Wu info @('os_name', $os.Caption)
        Out-Wu info @('os_build', $(if ($null -ne $ubr) { "$($os.BuildNumber).$ubr" } else { $os.BuildNumber }))
    } catch {}
    foreach ($u in $found.Updates) {
        $reboot = try { [int]$u.InstallationBehavior.RebootBehavior } catch { 0 }
        $released = try { ([datetime]$u.LastDeploymentChangeTime).ToString('yyyy-MM-dd') } catch { '' }
        Out-Wu update @(
            $u.Identity.UpdateID, (Get-WuKb $u), $u.Title, (Get-WuClass $u), $u.MsrcSeverity,
            [int64]$u.MaxDownloadSize, $released, $reboot, [int][bool]$u.IsDownloaded, [int][bool]$u.BrowseOnly
        )
    }
    # Recent installs, without the daily Defender intelligence updates and Store apps.
    $searcher = $found.Searcher
    $count = try { $searcher.GetTotalHistoryCount() } catch { 0 }
    if ($count -gt 0) {
        $n = 0
        foreach ($e in $searcher.QueryHistory(0, [Math]::Min($count, 200))) {
            if ($n -ge 40) { break }
            if ($e.Operation -ne 1 -or -not $e.Title -or $e.Title -match 'KB2267602|^[0-9A-Z]{12}-') { continue }
            $kb = [regex]::Match("$($e.Title)", 'KB\d+').Value
            Out-Wu history @((Get-IsoTime $e.Date), [int]$e.ResultCode, $kb, $e.Title)
            $n++
        }
    }
    Write-Output "Found $($found.Updates.Count) missing update(s)"
}

function Test-WuWanted($u) {
    $keys = @((Get-WuKb $u), "$($u.Identity.UpdateID)") | Where-Object { $_ }
    foreach ($x in $WuExclude) { if ($keys -contains $x) { return $false } }
    if ($WuKbs.Count) {
        foreach ($x in $WuKbs) { if ($keys -contains $x) { return $true } }
        return $false
    }
    # Optional updates (feature updates offered as "download and install now") only when named.
    if ($u.BrowseOnly) { return $false }
    if ($WuClasses -contains '*') { return $true }
    return $WuClasses -contains (Get-WuClass $u)
}

function Invoke-WuInstall {
    $found = Search-Wu
    $todo = @($found.Updates | Where-Object { Test-WuWanted $_ })
    $skipped = $found.Updates.Count - $todo.Count
    if (-not $todo.Count) {
        Write-Output "No updates to install ($($found.Updates.Count) missing, none selected)."
        Out-Wu result @(0, 0, $(if (Test-RebootPending) { 'true' } else { 'false' }), 0)
        return
    }
    Write-Output "$($todo.Count) update(s) to install$(if ($skipped) { ", $skipped not selected" }):"
    foreach ($u in $todo) { Write-Output "  $(Get-WuKb $u) $($u.Title)" }
    $installed = 0; $failed = 0; $reboot = $false; $i = 0
    foreach ($u in $todo) {
        $i++
        $label = "[$i/$($todo.Count)] $((@((Get-WuKb $u), $u.Title) | Where-Object { $_ }) -join ' ')"
        try {
            if (-not $u.EulaAccepted) { $u.AcceptEula() }
            $coll = New-Object -ComObject Microsoft.Update.UpdateColl
            [void]$coll.Add($u)
            if (-not $u.IsDownloaded) {
                Write-Output "$label - downloading ($([math]::Round([int64]$u.MaxDownloadSize / 1MB, 1)) MB)"
                $dl = $script:Session.CreateUpdateDownloader()
                $dl.Updates = $coll
                $dr = try { $dl.Download() } catch { throw "download failed: $(Format-WuError $_)" }
                if ($dr.ResultCode -notin 2, 3) { throw "download $($ResultNames[[int]$dr.ResultCode]) $(Format-Hresult $dr.HResult)" }
            }
            Write-Output "$label - installing"
            $inst = $script:Session.CreateUpdateInstaller()
            $inst.Updates = $coll
            $inst.AllowSourcePrompts = $false
            $ir = try { $inst.Install() } catch { throw "install failed: $(Format-WuError $_)" }
            $r = $ir.GetUpdateResult(0)
            if ($r.ResultCode -notin 2, 3) {
                $hint = Get-WuHint $r.HResult
                throw "install $($ResultNames[[int]$r.ResultCode]) $(Format-Hresult $r.HResult)$(if ($hint) { ": $hint" })"
            }
            $installed++
            $needs = [bool]($r.RebootRequired -or $ir.RebootRequired)
            if ($needs) { $reboot = $true }
            Write-Output "$label - installed$(if ($needs) { ' (restart required)' })"
        } catch {
            $failed++
            Write-Output "$label - FAILED: $($_.Exception.Message)"
        }
    }
    if (Test-RebootPending) { $reboot = $true }
    Write-Output "Installed $installed, failed $failed$(if ($reboot) { '; a restart is required' })"
    Out-Wu result @($installed, $failed, $(if ($reboot) { 'true' } else { 'false' }), $todo.Count)
}

$code = 0
try {
    if ($WuMode -eq 'install') { Invoke-WuInstall } else { Invoke-WuScan }
    Out-Wu done @()
} catch {
    Write-Output "ERROR: $($_.Exception.Message)"
    $code = 1
}
if ($WuExitFile) { [IO.File]::WriteAllText($WuExitFile, "$code") }
exit $code
