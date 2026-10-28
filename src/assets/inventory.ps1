# PSCCM inventory scan (Windows). Prints one "@@inv<TAB>kind<TAB>fields..." line per item.
# Non-ASCII characters and backslashes are written as \uXXXX so the text survives any
# console code page between the target and the console.
$ErrorActionPreference = 'Continue'

function Out-Inv {
    param([string]$Kind, [object[]]$Fields)
    $clean = foreach ($f in $Fields) {
        $s = ("$f" -replace '[\t\r\n]+', ' ').Trim()
        [regex]::Replace($s, '[^\x20-\x5b\x5d-\x7e]', { param($m) '\u{0:x4}' -f [int][char]$m.Value })
    }
    Write-Output ("@@inv`t$Kind`t" + ($clean -join "`t"))
}

function Fact {
    param([string]$Key, $Value)
    if ($null -ne $Value -and "$Value" -ne '') { Out-Inv 'fact' @($Key, $Value) }
}

function Get-Safe {
    param([scriptblock]$Block)
    try { & $Block } catch { $null }
}

$os = Get-Safe { Get-CimInstance Win32_OperatingSystem }
$cs = Get-Safe { Get-CimInstance Win32_ComputerSystem }
$bios = Get-Safe { Get-CimInstance Win32_BIOS }
$cpu = @(Get-Safe { Get-CimInstance Win32_Processor })
$cv = Get-Safe { Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction Stop }

if ($os) {
    Fact os_name $os.Caption
    Fact kernel $os.Version
    Fact arch $os.OSArchitecture
    Fact os_build $(if ($null -ne $cv.UBR) { "$($os.BuildNumber).$($cv.UBR)" } else { $os.BuildNumber })
    Fact last_boot (Get-Safe { $os.LastBootUpTime.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') })
    Fact install_date (Get-Safe { $os.InstallDate.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') })
}
if ($cv) { Fact os_version $(if ($cv.DisplayVersion) { $cv.DisplayVersion } else { $cv.ReleaseId }) }
if ($cs) {
    Fact manufacturer $cs.Manufacturer
    Fact model $cs.Model
    Fact domain $cs.Domain
    Fact user $cs.UserName
    Fact ram_mb ([math]::Round($cs.TotalPhysicalMemory / 1MB))
}
if ($bios) { Fact serial $bios.SerialNumber }
if ($cpu.Count) {
    Fact cpu $cpu[0].Name
    Fact cpu_count (($cpu | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum)
}
Fact ip_addresses ((@(Get-Safe { Get-CimInstance Win32_NetworkAdapterConfiguration -Filter 'IPEnabled=TRUE' }) |
    ForEach-Object { $_.IPAddress } | Where-Object { $_ -and $_ -notmatch ':' }) -join ', ')

$cbs = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending'
$wu = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired'
$active = Get-Safe { (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\ComputerName\ActiveComputerName' -ErrorAction Stop).ComputerName }
$pending = Get-Safe { (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\ComputerName\ComputerName' -ErrorAction Stop).ComputerName }
$rename = $active -and $pending -and ($active -ne $pending)
Fact pending_reboot $(if ($cbs -or $wu -or $rename) { 'true' } else { 'false' })

$hotfix = Get-Safe { Get-HotFix -ErrorAction Stop | Where-Object { $_.InstalledOn } | Sort-Object InstalledOn -Descending | Select-Object -First 1 }
if ($hotfix) {
    Fact last_patch ($hotfix.InstalledOn.ToString('yyyy-MM-dd'))
    Fact last_patch_id $hotfix.HotFixID
}

foreach ($d in @(Get-Safe { Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' })) {
    if ($d.Size) { Out-Inv 'disk' @($d.DeviceID, $d.Size, $d.FreeSpace, $d.FileSystem) }
}

# Installed programs as listed in "Programs and Features" (no system components or updates).
$keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
Get-ItemProperty -Path $keys -ErrorAction SilentlyContinue |
    Where-Object { $_.DisplayName -and $_.SystemComponent -ne 1 -and -not $_.ParentKeyName -and $_.ReleaseType -notin 'Update', 'Hotfix', 'Security Update' } |
    Sort-Object DisplayName, DisplayVersion -Unique |
    ForEach-Object { Out-Inv 'app' @($_.DisplayName, $_.DisplayVersion, $_.Publisher) }

Write-Output "@@inv`tdone"
exit 0
