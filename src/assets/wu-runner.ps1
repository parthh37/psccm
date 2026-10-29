# Runs $WuScript (set above, see windows-update.ps1) as SYSTEM and passes its output on.
# The Windows Update Agent refuses to download or install updates for a remote (WinRM)
# session, so unless this already runs as SYSTEM (the agent), the script runs as a one-off
# scheduled task and its output is read back from a log file while it runs.
$dir = Join-Path $env:windir 'Temp\psccm'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$id = [guid]::NewGuid().ToString('N').Substring(0, 12)
$file = Join-Path $dir "wu-$id.ps1"
$log = Join-Path $dir "wu-$id.log"
$done = Join-Path $dir "wu-$id.exit"
$bom = New-Object Text.UTF8Encoding $true
$psExe = Join-Path $env:windir 'System32\WindowsPowerShell\v1.0\powershell.exe'

if ([Security.Principal.WindowsIdentity]::GetCurrent().IsSystem) {
    [IO.File]::WriteAllText($file, $WuScript, $bom)
    try {
        & $psExe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $file
        $code = $LASTEXITCODE
    } finally {
        Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
    }
    exit $code
}

$taskPath = '\PSCCM\'
$old = Get-ScheduledTask -TaskPath $taskPath -TaskName $WuTask -ErrorAction SilentlyContinue
if ($old -and $old.State -eq 'Running') {
    Write-Output "Another PSCCM Windows Update $WuMode is still running on this computer (task $taskPath$WuTask); try again when it has finished."
    exit 1
}
[IO.File]::WriteAllText($file, "`$WuExitFile = '$done'`r`n" + $WuScript, $bom)
$cmdArgs = '/d /s /c ""' + $psExe + '" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $file + '" > "' + $log + '" 2>&1"'
$action = New-ScheduledTaskAction -Execute (Join-Path $env:windir 'System32\cmd.exe') -Argument $cmdArgs
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 8) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskPath $taskPath -TaskName $WuTask -Action $action -Principal $principal -Settings $settings -Force | Out-Null

$script:pos = 0
$script:carry = ''
$decoder = (New-Object Text.UTF8Encoding $false).GetDecoder()
# Print the complete lines the task has written since the last call.
function Read-WuLog([switch]$Final) {
    if (Test-Path -LiteralPath $log) {
        $fs = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
        try {
            $len = $fs.Length
            if ($len -gt $script:pos) {
                $fs.Position = $script:pos
                $buf = New-Object byte[] ($len - $script:pos)
                $n = $fs.Read($buf, 0, $buf.Length)
                $script:pos += $n
                $chars = New-Object char[] ($n + 4)
                $c = $decoder.GetChars($buf, 0, $n, $chars, 0)
                $script:carry += New-Object string ($chars, 0, $c)
            }
        } finally { $fs.Close() }
    }
    $lines = $script:carry -split "`r?`n"
    $script:carry = $lines[-1]
    if ($lines.Count -gt 1) { $lines[0..($lines.Count - 2)] }
    if ($Final -and $script:carry) { $script:carry; $script:carry = '' }
}

$code = 1
try {
    Start-ScheduledTask -TaskPath $taskPath -TaskName $WuTask
    $started = Get-Date
    while ($true) {
        Start-Sleep -Seconds 2
        Read-WuLog
        if (Test-Path -LiteralPath $done) { break }
        $state = (Get-ScheduledTask -TaskPath $taskPath -TaskName $WuTask).State
        if ("$state" -notin 'Running', 'Queued' -and ((Get-Date) - $started).TotalSeconds -gt 20) { break }
    }
    Start-Sleep -Milliseconds 500
    Read-WuLog -Final
    if (Test-Path -LiteralPath $done) {
        $code = [int]("$(Get-Content -LiteralPath $done -Raw)".Trim())
    } else {
        $info = Get-ScheduledTaskInfo -TaskPath $taskPath -TaskName $WuTask
        Write-Output ("The Windows Update task ended without a result (task result 0x{0:X8})" -f $info.LastTaskResult)
    }
} finally {
    Unregister-ScheduledTask -TaskPath $taskPath -TaskName $WuTask -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $file, $log, $done -Force -ErrorAction SilentlyContinue
}
exit $code
