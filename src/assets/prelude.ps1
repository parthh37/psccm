# ---- PSCCM helpers (available in every PowerShell step) ----
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# The console decodes step output as UTF-8.
try { [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false } catch {}
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

# Install (/i), patch (/p) or remove (/x, -Path = product code) an MSI quietly
# with a verbose log; returns the exit code and, on failure, prints the log
# lines that say why.
function Invoke-PsccmMsi {
    param([Parameter(Mandatory)][string]$Path, [string]$Arguments, [string]$Verb = '/i', [string]$Log)
    if (-not $Log) { $Log = if ($Verb -eq '/x') { Join-Path $env:TEMP "psccm-msi-$($Path.Trim('{}')).log" } else { "$Path.log" } }
    $code = Invoke-PsccmProcess -FilePath 'msiexec.exe' -Arguments "$Verb `"$Path`" /qn /norestart /l*v `"$Log`" $Arguments"
    if ($code -notin 0, 3010, 1641 -and (Test-Path $Log)) {
        Write-Host "--- MSI log: $Log ---"
        $lines = @(Get-Content -LiteralPath $Log)
        $hits = @($lines | Select-String -Pattern 'Product: .* -- |Error \d{4}|error code \d|later version|newer version' | Select-Object -First 15)
        if ($hits.Count) { $hits | ForEach-Object { Write-Host $_.Line } } else { $lines | Select-Object -Last 25 | ForEach-Object { Write-Host $_ } }
    }
    return $code
}

# Uninstall every app matching -Name using its MSI product code or uninstall string.
function Uninstall-App {
    param([Parameter(Mandatory)][string]$Name, [string]$ExeArgs = '/S')
    foreach ($a in @(Get-InstalledApp -Name $Name)) {
        Write-Host "Uninstalling $($a.DisplayName) $($a.DisplayVersion)"
        if ($a.PSChildName -match '^\{[0-9A-Fa-f-]+\}$') {
            $code = Invoke-PsccmMsi -Verb '/x' -Path $a.PSChildName
        } else {
            $exe = ($a.UninstallString -replace '^"([^"]+)".*$', '$1')
            $code = Invoke-PsccmProcess -FilePath $exe -Arguments $ExeArgs
        }
        if ($code -notin 0, 3010, 1605) { throw "Uninstall of $($a.DisplayName) failed with exit code $code" }
    }
}
# ---- end of helpers ----
