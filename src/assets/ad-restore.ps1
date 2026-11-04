# PSCCM Active Directory restore. Runs on a domain controller as one step of
# an "AD restore" job, after the plan was reviewed and confirmed twice in the
# console. Set before this script: $planFile, $zipFile ('' = no GPOs),
# $initialPassword ('' = random password and the account stays disabled).
#
# The plan is JSON Lines: a header, then one operation per line. Every
# operation is tried on its own; one failure does not stop the others.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$lines = [IO.File]::ReadAllLines($planFile, [Text.Encoding]::UTF8)
$plan = $lines[0] | ConvertFrom-Json
$ops = @(for ($i = 1; $i -lt $lines.Length; $i++) { if ($lines[$i].Trim()) { $lines[$i] | ConvertFrom-Json } })
$gpoOps = @($ops | Where-Object { $_.op -eq 'gpo' })
$adOps = @($ops | Where-Object { $_.op -ne 'gpo' })

$script:ok = 0; $script:failed = 0; $script:skipped = 0
function Done([string]$what) { $script:ok++; Write-Output "OK       $what" }
function Fail([string]$what, $err) { $script:failed++; Write-Output "FAILED   ${what}: $err" }
function Skip([string]$what, [string]$why) { $script:skipped++; Write-Output "SKIPPED  ${what}: $why" }

# ---------------------------------------------------------------- safety check
try {
    Import-Module ActiveDirectory
    $dom = Get-ADDomain
} catch {
    Write-Output "ERROR: the ActiveDirectory PowerShell module is not available here ($($_.Exception.Message)). Run the restore on a domain controller. Nothing was changed."
    exit 1
}
if ($dom.DNSRoot -ne $plan.expect_domain -or $dom.DistinguishedName -ne $plan.target_root) {
    Write-Output "ERROR: this computer is in domain $($dom.DNSRoot) ($($dom.DistinguishedName)), but the restore was confirmed for $($plan.expect_domain) ($($plan.target_root)). Nothing was changed."
    exit 1
}
$srv = $null
try { $srv = (Get-ADDomainController -Server $env:COMPUTERNAME).HostName } catch {}
if (-not $srv) { $srv = [string](Get-ADDomainController -Discover).HostName }
# Every read and write goes to the same DC, so later steps see earlier ones.
$PSDefaultParameterValues['*-AD*:Server'] = $srv
$PSDefaultParameterValues['*-GP*:Server'] = $srv
$PSDefaultParameterValues['Restore-GPO:Server'] = $srv
$PSDefaultParameterValues['Import-GPO:Server'] = $srv
Write-Output "Restore $($plan.plan_id): $($plan.mode) of $($adOps.Count) AD operation(s) and $($gpoOps.Count) GPO(s) in $($dom.DNSRoot) on $srv"
try {
    $rb = Get-ADOptionalFeature -Filter "Name -eq 'Recycle Bin Feature'"
    $state = if (@($rb.EnabledScopes).Count) { 'on' } else { 'off (deleted objects come back without most attributes; PSCCM re-applies them from the backup)' }
    Write-Output "AD Recycle Bin: $state"
} catch {}

# -------------------------------------------------------------------- helpers
$intAttrs = @('useraccountcontrol', 'grouptype', 'countrycode', 'gpoptions', 'primarygroupid')
$longAttrs = @('accountexpires')
$accountClasses = @('user', 'inetOrgPerson', 'computer')

# Backup attributes (name -> array of strings) as a hashtable for -Replace,
# with numbers typed and single values unwrapped.
function To-Hash($attrs, [string[]]$skip = @()) {
    $h = @{}
    if ($null -eq $attrs) { return $h }
    foreach ($p in $attrs.PSObject.Properties) {
        $n = $p.Name
        if ($skip -contains $n) { continue }
        $v = @($p.Value | Where-Object { $null -ne $_ })
        if ($v.Count -eq 0) { continue }
        if ($intAttrs -contains $n) { $v = @($v | ForEach-Object { [int]$_ }) }
        elseif ($longAttrs -contains $n) { $v = @($v | ForEach-Object { [long]$_ }) }
        else { $v = @($v | ForEach-Object { [string]$_ }) }
        if ($v.Count -eq 1) { $h[$n] = $v[0] } else { $h[$n] = [object[]]$v }
    }
    return $h
}

function First($attrs, [string]$name) {
    if ($null -eq $attrs) { return $null }
    $p = $attrs.PSObject.Properties[$name]
    if ($p) { return @($p.Value)[0] }
    return $null
}

# Objects re-created in this run: old GUID -> new DN.
$recreated = @{}
function Find-Guid([string]$g) {
    if ($recreated.ContainsKey($g)) { return $recreated[$g] }
    try { return (Get-ADObject -Identity $g).DistinguishedName } catch { return $null }
}
function Find-Dn([string]$dn) {
    try { return (Get-ADObject -Identity $dn).DistinguishedName } catch { return $null }
}

# "CN=Smith\, John,OU=Users,DC=x" -> "OU=Users,DC=x"
function Parent-Of([string]$dn) { return ($dn -replace '^(?:\\.|[^,\\])*,', '') }

function New-Password {
    $chars = [char[]](33..126) | Where-Object { $_ -notin @([char]'"', [char]"'", [char]'`') }
    return (-join ($chars | Get-Random -Count 24)) + 'aZ7!'
}

# Create an object from its backup. Accounts get a password first, then their
# userAccountControl, because AD refuses to enable an account without one.
function New-FromBackup($o, [string]$parent) {
    $attrs = To-Hash $o.attrs @('useraccountcontrol', 'pwdlastset', 'manager', 'managedby')
    $params = @{ Name = $o.name; Type = $o.class; Path = $parent; PassThru = $true }
    if ($attrs.Count) { $params.OtherAttributes = $attrs }
    $new = New-ADObject @params
    if ($accountClasses -contains $o.class) {
        $uac = [int](First $o.attrs 'useraccountcontrol')
        $isComputer = $o.class -eq 'computer'
        if (-not $uac) { $uac = if ($isComputer) { 4096 } else { 512 } }
        $pw = if ($initialPassword -and -not $isComputer) { $initialPassword } else { New-Password }
        Set-ADAccountPassword -Identity $new.ObjectGUID -Reset -NewPassword (ConvertTo-SecureString $pw -AsPlainText -Force)
        if (-not $isComputer -and -not $initialPassword) { $uac = $uac -bor 2 }
        Set-ADObject -Identity $new.ObjectGUID -Replace @{ userAccountControl = $uac }
        # "Must change password at next logon" (not possible with "password never expires").
        if (-not $isComputer -and -not ($uac -band 0x10000)) {
            try { Set-ADObject -Identity $new.ObjectGUID -Replace @{ pwdLastSet = 0 } } catch {}
        }
    }
    return $new.DistinguishedName
}

function Add-ToGroup([string]$groupDn, [string]$memberDn) {
    $m = Get-ADObject -Identity $memberDn -Properties memberOf
    if (@($m.memberOf) -contains $groupDn) { return $false }
    Set-ADObject -Identity $groupDn -Add @{ member = $memberDn }
    return $true
}

# Set attributes, then (separately) userAccountControl, so a refused account
# state does not block the rest.
function Set-FromBackup([string]$id, $replace, [string[]]$clear, [string]$what) {
    $h = To-Hash $replace @('useraccountcontrol')
    if ($h.Count) { Set-ADObject -Identity $id -Replace $h }
    if (@($clear).Count) { Set-ADObject -Identity $id -Clear $clear }
    $uac = First $replace 'useraccountcontrol'
    if ($null -ne $uac) {
        try { Set-ADObject -Identity $id -Replace @{ userAccountControl = [int]$uac } }
        catch { Write-Output "WARNING  ${what}: account state not set ($($_.Exception.Message))" }
    }
}

# ------------------------------------------------------------- AD operations
foreach ($o in $adOps) {
    $what = "$($o.op) $($o.class) $($o.name)"
    try {
        switch ($o.op) {
            'undelete' {
                $what = "restore deleted $($o.class) '$($o.name)'"
                $d = $null
                try { $d = Get-ADObject -Identity $o.guid -IncludeDeletedObjects -Properties isDeleted } catch {}
                if ($d -and -not $d.isDeleted) { Skip $what 'it is not deleted (restored already?)'; break }
                if (-not (Find-Dn $o.parent)) { Fail $what "its OU/container $($o.parent) does not exist"; break }
                if ($d) {
                    Restore-ADObject -Identity $d.ObjectGUID -NewName $o.name -TargetPath $o.parent
                    Set-FromBackup $o.guid $o.attrs @() $what
                    $dn = Find-Guid $o.guid
                    Done "$what (from the Recycle Bin, same SID)"
                } elseif ($o.recreate) {
                    $dn = New-FromBackup $o $o.parent
                    $recreated[$o.guid] = $dn
                    Done "$what (re-created: new SID)"
                } else {
                    Fail $what 'not in the AD Recycle Bin any more, and re-creating was not chosen'
                    break
                }
                foreach ($g in @($o.groups)) {
                    $gdn = Find-Guid $g.guid
                    if (-not $gdn) { Skip "add '$($o.name)' to group '$($g.name)'" 'group not found'; continue }
                    try { if (Add-ToGroup $gdn $dn) { Done "add '$($o.name)' to group '$($g.name)'" } }
                    catch { Fail "add '$($o.name)' to group '$($g.name)'" $_.Exception.Message }
                }
            }
            'move' {
                $what = "move $($o.class) '$($o.name)' back to $($o.parent)"
                $dn = Find-Guid $o.guid
                if (-not $dn) { Fail $what 'object not found'; break }
                if ($o.parent -and ((Parent-Of $dn) -ne $o.parent)) {
                    Move-ADObject -Identity $o.guid -TargetPath $o.parent
                }
                $cur = Get-ADObject -Identity $o.guid
                if ($cur.Name -cne $o.name) { Rename-ADObject -Identity $o.guid -NewName $o.name }
                Done $what
            }
            'set' {
                $what = "revert attributes of $($o.class) '$($o.name)'"
                if (-not (Find-Guid $o.guid)) { Fail $what 'object not found'; break }
                Set-FromBackup $o.guid $o.replace @($o.clear) $what
                Done "$what ($((@($o.replace.PSObject.Properties).Count + @($o.clear).Count)) attribute(s))"
            }
            'members' {
                $gdn = Find-Guid $o.guid
                if (-not $gdn) { Fail "group members of '$($o.name)'" 'group not found'; break }
                foreach ($m in @($o.add)) {
                    $w = "add '$($m.name)' to group '$($o.name)'"
                    $mdn = Find-Guid $m.guid
                    if (-not $mdn) { Skip $w 'member not found'; continue }
                    try { if (Add-ToGroup $gdn $mdn) { Done $w } else { Skip $w 'already a member' } } catch { Fail $w $_.Exception.Message }
                }
                foreach ($m in @($o.remove)) {
                    $w = "remove '$($m.name)' from group '$($o.name)'"
                    $mdn = Find-Guid $m.guid
                    if (-not $mdn) { Skip $w 'member not found'; continue }
                    try { Set-ADObject -Identity $gdn -Remove @{ member = $mdn }; Done $w } catch { Fail $w $_.Exception.Message }
                }
            }
            'ensure' {
                $what = "$($o.class) '$($o.name)' ($($o.dn))"
                if (Find-Dn $o.dn) {
                    if ($plan.options.update_existing) {
                        Set-FromBackup $o.dn $o.attrs @() $what
                        Done "update existing $what"
                    } else { Skip $what 'exists already (left unchanged)' }
                } elseif (-not (Find-Dn $o.parent)) {
                    Fail "create $what" "its OU/container $($o.parent) does not exist"
                } else {
                    [void](New-FromBackup $o $o.parent)
                    Done "create $what"
                }
            }
            'refs' {
                $what = "set references of '$($o.name)'"
                if (-not (Find-Dn $o.dn)) { Skip $what 'object not found'; break }
                $h = @{}
                foreach ($p in $o.replace.PSObject.Properties) {
                    $vals = @($p.Value | Where-Object { Find-Dn $_ })
                    if ($vals.Count -eq 1) { $h[$p.Name] = [string]$vals[0] } elseif ($vals.Count) { $h[$p.Name] = [object[]]$vals }
                }
                if ($h.Count) { Set-ADObject -Identity $o.dn -Replace $h; Done "$what ($($h.Keys -join ', '))" }
                else { Skip $what 'the referenced objects do not exist' }
            }
            'members_dn' {
                if (-not (Find-Dn $o.dn)) { Skip "members of group '$($o.name)'" 'group not found'; break }
                $added = 0; $missing = 0
                foreach ($mdn in @($o.add)) {
                    if (-not (Find-Dn $mdn)) { $missing++; continue }
                    try { if (Add-ToGroup $o.dn $mdn) { $added++ } } catch { Fail "add $mdn to group '$($o.name)'" $_.Exception.Message }
                }
                Done "group '$($o.name)': $added member(s) added$(if ($missing) { ", $missing not found" })"
            }
            default { Skip $what 'unknown operation' }
        }
    } catch {
        Fail $what $_.Exception.Message
    }
}

# ---------------------------------------------------------------------- GPOs
if ($gpoOps.Count) {
    $gdir = Join-Path ([IO.Path]::GetTempPath()) ('psccm-gpo-' + [guid]::NewGuid().ToString('N'))
    try {
        Import-Module GroupPolicy
        Expand-Archive -LiteralPath $zipFile -DestinationPath $gdir
        foreach ($g in $gpoOps) {
            $what = if ($plan.mode -eq 'revert') { "restore GPO '$($g.name)'" } else { "import GPO '$($g.name)'" }
            try {
                $note = ''
                if ($plan.mode -eq 'revert') {
                    $exists = $true
                    try { [void](Get-GPO -Guid $g.guid) } catch { $exists = $false }
                    if ($exists) {
                        $r = Restore-GPO -BackupId $g.backup_id -Path $gdir -Confirm:$false
                    } else {
                        # Deleted since the backup: bring it back under its old name.
                        $r = Import-GPO -BackupId $g.backup_id -Path $gdir -TargetName $g.name -CreateIfNeeded
                        $note = ' (it had been deleted: re-created with a new GUID, permissions reset to the defaults)'
                    }
                } else {
                    $r = Import-GPO -BackupId $g.backup_id -Path $gdir -TargetName $g.name -CreateIfNeeded
                }
                $id = $r.Id
                Done "$what$note"
            } catch { Fail $what $_.Exception.Message; continue }
            if (-not $plan.options.restore_links) { continue }
            foreach ($l in @($g.links)) {
                $w = "link GPO '$($g.name)' to $($l.target)"
                try {
                    $en = if ($l.enabled) { 'Yes' } else { 'No' }
                    $ef = if ($l.enforced) { 'Yes' } else { 'No' }
                    $have = @((Get-GPInheritance -Target $l.target).GpoLinks | Where-Object { $_.GpoId -eq $id })
                    if ($have.Count) { Set-GPLink -Guid $id -Target $l.target -LinkEnabled $en -Enforced $ef | Out-Null; Done "$w (link settings)" }
                    else { New-GPLink -Guid $id -Target $l.target -LinkEnabled $en -Enforced $ef | Out-Null; Done $w }
                } catch { Fail $w $_.Exception.Message }
            }
        }
    } catch {
        Fail 'GPO restore' $_.Exception.Message
    } finally {
        Remove-Item -LiteralPath $gdir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Output "Restore finished: $($script:ok) done, $($script:skipped) skipped, $($script:failed) failed"
if ($script:failed) { exit 1 }
exit 0
