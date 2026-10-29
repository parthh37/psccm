# ---- PSCCM helpers (available in every PowerShell step) ----
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# The console decodes step output as UTF-8.
try { [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false } catch {}
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

# Store / MSIX apps installed for any user or provisioned for new users (main packages only:
# no frameworks, resource packages or Windows system apps). -Name matches the display name or
# the package name ('Microsoft Teams' or 'MSTeams'). Throws when the packages cannot be read.
function Get-InstalledAppx {
    param([string]$Name = '*')
    if (-not (Get-Command Get-AppxPackage -ErrorAction SilentlyContinue)) { return }
    # Display name and publisher from the manifest; 'ms-resource:' names fall back to the package's.
    function Read-Manifest($File, $Display, $Publisher) {
        if ($File -and (Test-Path -LiteralPath $File)) {
            try {
                $p = ([xml](Get-Content -LiteralPath $File -Raw)).Package.Properties
                if ($p.DisplayName -and $p.DisplayName -notlike 'ms-resource:*') { $Display = $p.DisplayName }
                if ($p.PublisherDisplayName -and $p.PublisherDisplayName -notlike 'ms-resource:*') { $Publisher = $p.PublisherDisplayName }
            } catch {}
        }
        return $Display, $Publisher
    }
    $known = @{}
    foreach ($p in @(Get-AppxPackage -AllUsers -PackageTypeFilter Main -ErrorAction Stop)) {
        if ($p.IsFramework -or $p.NonRemovable -or "$($p.SignatureKind)" -eq 'System') { continue }
        $cn = $p.Publisher -replace '^CN="?([^,"]+)"?.*$', '$1'
        $display, $publisher = Read-Manifest (Join-Path "$($p.InstallLocation)" 'AppxManifest.xml') $p.Name $cn
        $known[$p.Name] = $display, $publisher
        if ($display -notlike $Name -and $p.Name -notlike $Name) { continue }
        [pscustomobject]@{ DisplayName = $display; DisplayVersion = "$($p.Version)"; Publisher = $publisher
            PackageName = $p.Name; PackageFullName = $p.PackageFullName; Provisioned = $false }
    }
    foreach ($p in @(Get-AppxProvisionedPackage -Online -ErrorAction Stop)) {
        $display, $publisher = if ($known.ContainsKey($p.DisplayName)) { $known[$p.DisplayName] } else {
            Read-Manifest ([Environment]::ExpandEnvironmentVariables("$($p.InstallLocation)")) $p.DisplayName ''
        }
        if ($display -notlike $Name -and $p.DisplayName -notlike $Name) { continue }
        [pscustomobject]@{ DisplayName = $display; DisplayVersion = "$($p.Version)"; Publisher = $publisher
            PackageName = $p.DisplayName; PackageFullName = $p.PackageName; Provisioned = $true }
    }
}

# Installed programs from the registry (64- and 32-bit) and Store / MSIX apps (unless
# -NoStore), filtered by display name wildcard.
function Get-InstalledApp {
    param([string]$Name = '*', [switch]$NoStore)
    $keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    Get-ItemProperty -Path $keys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -and $_.DisplayName -like $Name } |
        Select-Object DisplayName, DisplayVersion, Publisher, UninstallString, PSChildName
    if (-not $NoStore) {
        try { Get-InstalledAppx -Name $Name } catch { Write-Warning "Store apps could not be read: $($_.Exception.Message)" }
    }
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
    # The registry first: reading Store apps takes a few seconds.
    foreach ($store in $false, $true) {
        $apps = if ($store) { @(Get-InstalledApp -Name $Name | Where-Object PackageFullName) } else { @(Get-InstalledApp -Name $Name -NoStore) }
        foreach ($a in $apps) {
            if (-not $MinVersion -or (ConvertTo-PsccmVersion $a.DisplayVersion) -ge (ConvertTo-PsccmVersion $MinVersion)) { return $true }
        }
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

# Provision an MSIX / Appx package or bundle for every user: current users get the app at
# their next sign-in, new users at their first. -Arguments: dependency packages and an
# optional license .xml, separated by ';'. Relative paths are next to -Path; a folder or .zip
# adds every package in it that fits this computer. Returns 0 or 1.
function Install-PsccmAppx {
    param([Parameter(Mandatory)][string]$Path, [string]$Arguments)
    $Path = $Path.Replace('/', '\')
    # Architectures this computer can run, for picking dependencies out of a folder.
    $runs = switch ($env:PROCESSOR_ARCHITECTURE) { 'ARM64' { 'arm64', 'arm', 'x64', 'x86' } 'x86' { 'x86' } default { 'x64', 'x86' } }
    $deps = @(); $license = $null
    foreach ($item in @("$Arguments" -split ';' | ForEach-Object { $_.Trim().Trim('"') } | Where-Object { $_ })) {
        $item = $item.Replace('/', '\')
        if (-not [IO.Path]::IsPathRooted($item)) { $item = Join-Path (Split-Path $Path) $item }
        if ($item -like '*.xml') { $license = $item; continue }
        if ($item -like '*.zip') {
            $dir = "$item-files"
            Expand-Archive -LiteralPath $item -DestinationPath $dir -Force
            $item = $dir
        }
        if (Test-Path -LiteralPath $item -PathType Container) {
            $deps += Get-ChildItem -LiteralPath $item -Recurse -File |
                Where-Object { $_.Extension -in '.appx', '.msix', '.appxbundle', '.msixbundle' } |
                Where-Object { $_.Name -notmatch '[_.-](x86|x64|arm|arm64)[_.-]' -or $Matches[1] -in $runs } |
                ForEach-Object FullName
        } else {
            $deps += $item
        }
    }
    $params = @{ Online = $true; PackagePath = $Path }
    if ($deps.Count) { $params.DependencyPackagePath = $deps }
    if ($license) { $params.LicensePath = $license } else { $params.SkipLicense = $true }
    Write-Host "Provisioning $Path for all users"
    foreach ($d in $deps) { Write-Host "  dependency: $d" }
    if ($license) { Write-Host "  license: $license" }
    try {
        Add-AppxProvisionedPackage @params | Out-Null
        return 0
    } catch {
        Write-Host "Add-AppxProvisionedPackage failed: $($_.Exception.Message)"
        return 1
    }
}

# Uninstall every app matching -Name using its MSI product code, uninstall string or, for a
# Store / MSIX app, by removing it for all users and from the provisioned apps.
function Uninstall-App {
    param([Parameter(Mandatory)][string]$Name, [string]$ExeArgs = '/S')
    foreach ($a in @(Get-InstalledApp -Name $Name)) {
        Write-Host "Uninstalling $($a.DisplayName) $($a.DisplayVersion)"
        if ($a.PackageFullName) {
            if ($a.Provisioned) {
                Remove-AppxProvisionedPackage -Online -PackageName $a.PackageFullName -ErrorAction Stop | Out-Null
            } else {
                Remove-AppxPackage -Package $a.PackageFullName -AllUsers -ErrorAction Stop
            }
            continue
        }
        if ($a.PSChildName -match '^\{[0-9A-Fa-f-]+\}$') {
            $code = Invoke-PsccmMsi -Verb '/x' -Path $a.PSChildName
        } elseif ($a.UninstallString -match '^"([^"]*\\OfficeClickToRun\.exe)"\s*(.*)$') {
            # Microsoft 365 Apps / Office Click-to-Run: its own arguments, without any UI.
            $code = Invoke-PsccmProcess -FilePath $Matches[1] -Arguments "$($Matches[2]) DisplayLevel=False"
        } else {
            $exe = ($a.UninstallString -replace '^"([^"]+)".*$', '$1')
            $code = Invoke-PsccmProcess -FilePath $exe -Arguments $ExeArgs
        }
        if ($code -notin 0, 3010, 1605) { throw "Uninstall of $($a.DisplayName) failed with exit code $code" }
    }
}
# ---- end of helpers ----
