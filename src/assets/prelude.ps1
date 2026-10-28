# ---- PSCCM helpers (available in every PowerShell step) ----
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

# Installed programs from the registry (64- and 32-bit), filtered by display name wildcard.
function Get-InstalledApp {
    param([string]$Name = '*')
    $keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    Get-ItemProperty -Path $keys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -and $_.DisplayName -like $Name } |
        Select-Object DisplayName, DisplayVersion, Publisher, UninstallString, PSChildName
}

# Lenient version parse: '24.09', 'v22.11.0', '8.7.1 (64-bit)' -> [version].
function ConvertTo-PsccmVersion {
    param([string]$Version)
    $m = [regex]::Match("$Version", '\d+(\.\d+){0,3}')
    if (-not $m.Success) { return [version]'0.0' }
    $v = $m.Value
    if ($v -notmatch '\.') { $v += '.0' }
    return [version]$v
}

# True when an app matching -Name is installed, optionally at -MinVersion or newer.
function Test-AppInstalled {
    param([Parameter(Mandatory)][string]$Name, [string]$MinVersion)
    $apps = @(Get-InstalledApp -Name $Name)
    if ($apps.Count -eq 0) { return $false }
    if (-not $MinVersion) { return $true }
    foreach ($a in $apps) {
        if ((ConvertTo-PsccmVersion $a.DisplayVersion) -ge (ConvertTo-PsccmVersion $MinVersion)) { return $true }
    }
    return $false
}

function Get-PsccmFile {
    param([Parameter(Mandatory)][string]$Url, [Parameter(Mandatory)][string]$OutFile)
    Write-Host "Downloading $Url"
    New-Item -ItemType Directory -Force -Path (Split-Path $OutFile) | Out-Null
    Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
}

# Start a process, wait for it, and return its exit code.
function Invoke-PsccmProcess {
    param([Parameter(Mandatory)][string]$FilePath, [string]$Arguments)
    Write-Host "Running: $FilePath $Arguments"
    if ($Arguments) {
        $p = Start-Process -FilePath $FilePath -ArgumentList $Arguments -Wait -PassThru -NoNewWindow
    } else {
        $p = Start-Process -FilePath $FilePath -Wait -PassThru -NoNewWindow
    }
    return $p.ExitCode
}

# Uninstall every app matching -Name using its MSI product code or uninstall string.
function Uninstall-App {
    param([Parameter(Mandatory)][string]$Name, [string]$ExeArgs = '/S')
    foreach ($a in @(Get-InstalledApp -Name $Name)) {
        Write-Host "Uninstalling $($a.DisplayName) $($a.DisplayVersion)"
        if ($a.PSChildName -match '^\{[0-9A-Fa-f-]+\}$') {
            $code = Invoke-PsccmProcess -FilePath 'msiexec.exe' -Arguments "/x $($a.PSChildName) /qn /norestart"
        } else {
            $exe = ($a.UninstallString -replace '^"([^"]+)".*$', '$1')
            $code = Invoke-PsccmProcess -FilePath $exe -Arguments $ExeArgs
        }
        if ($code -notin 0, 3010, 1605) { throw "Uninstall of $($a.DisplayName) failed with exit code $code" }
    }
}
# ---- end of helpers ----
