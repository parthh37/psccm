# PSCCM — lightweight agentless deployment manager

[![CI](https://github.com/parthh37/psccm/actions/workflows/ci.yml/badge.svg)](https://github.com/parthh37/psccm/actions/workflows/ci.yml)
[![Release](https://github.com/parthh37/psccm/actions/workflows/release.yml/badge.svg)](https://github.com/parthh37/psccm/releases)

A single Rust executable that deploys software and runs changes on **Windows and Linux**
computers without installing an agent, similar to PDQ Deploy.

- **Windows** targets over **WinRM** (PowerShell remoting, Kerberos/NTLM, encrypted).
- **Linux** targets over **SSH** (password or key, `sudo`), with host-key pinning.
- **Active Directory** computer import that is **read-only**: it only runs LDAP searches.
- **Computer groups**: static lists or dynamic rules (OS, name pattern, OU, AD security group, tag,
  installed software and version, pending reboot, low disk space).
- **Inventory**: agentless scans of OS, hardware, disks, logged-on user, pending reboot and installed
  software, with a software report across all computers, outdated App catalog software with one-click
  update, a change history (installed / removed / updated), uninstall from the report and CSV export.
- **Windows Update**: scan for missing updates (from WSUS or Windows Update), decline problem KBs, and
  install with automatic restarts on a monthly Patch Tuesday schedule, with no agent needed.
- **Group Policy import and injection**: read GPOs from AD/SYSVOL (read-only) or a GPO backup, and
  apply their computer settings directly on chosen computers, with no GPO link or AD change.
- **Optional agent** for computers with no WinRM/SSH: it connects out to the console and adds live
  CPU/disk analytics, on-demand event-log capture, Task Scheduler, firewall and local-user
  management, and an in-browser terminal — all with nothing opened inbound.
- **Web console** for everyday use, plus a **CLI** for scripts and CI pipelines.
- **YAML packages** with detection, variables, conditions, retries, rollback, rolling
  batches and steps delegated to another host (e.g. drain HAProxy while patching IIS).
- **App catalog**: 7-Zip, Chrome, Firefox, Node.js, Python, Java, VS Code, Git and more at
  their latest version, downloaded once, checksum-verified and kept up to date automatically.
- **Microsoft 365 Apps, new Teams and Store / MSIX apps**: Office through the Office Deployment
  Tool, Teams through Microsoft's bootstrapper, and any `.msix`/`.appx` provisioned for all users.
  Store apps show up in inventory and can be uninstalled from it.
- **Windows installer** (Inno Setup) that runs PSCCM as a service, plus **auto-update** from
  GitHub releases (SHA-256 verified).

Built-in packages: 7-Zip, Google Chrome, Notepad++, Node.js install/upgrade, Microsoft 365
Apps (Office), Microsoft Teams (new), IIS install, IIS site deploy, IIS app-pool recycle, IIS
zero-downtime deploy behind HAProxy, HAProxy install/upgrade, HAProxy config deploy, HAProxy
drain/ready, generic MSI/EXE/MSIX install, Windows uninstall, Windows Update, Linux package
install/remove/upgrade, Linux full update, and reboot.

## Why not Ansible?

The Ansible control node does not run on Windows (WSL only) and needs Python. PSCCM runs as
one `.exe` on the admin box you already have and uses the remoting stack built into
Windows. Being agentless, it needs nothing on the targets beyond WinRM or SSH.

---

## Install

**Windows (recommended):** download `psccm-setup-<version>.exe` from
[Releases](https://github.com/parthh37/psccm/releases) and run it. The setup asks for:

- **Port** (default 8080).
- **Network access**: this computer only, or other computers too. For network access a random
  access token is generated and shown at the end.
- **Service account**: the Windows account the `PSCCM` service runs as. Use a domain account that is
  local admin on your targets (e.g. `CORP\svc-psccm`), so Windows deployments use Kerberos with no
  stored password. Leave it empty for Local System and add a Windows credential in the console.

Files go to `C:\Program Files\PSCCM`, data to `C:\ProgramData\PSCCM`. Upgrades keep the service
account and settings.

**Portable:** download `psccm-x86_64-windows.exe` (or build with `cargo build --release`), put it in a
folder and double-click it. Data is kept in `data\` next to the executable and the console opens at
<http://127.0.0.1:8080/>.

**Linux console:** `psccm-x86_64-linux` plus [packaging/psccm.service](packaging/psccm.service) for systemd.
Windows targets need a Windows console (PowerShell remoting); Linux targets work from either.

| Data folder | What |
|---|---|
| `psccm.db` | Computers, groups, credentials (encrypted), history, schedules, inventory, settings |
| `packages\` | Package YAML files (the built-ins are written here on first start) |
| `repository\` | Installers and files that packages push (`install: file`, `copy: src`) |
| `gpos\` | Imported Group Policy snapshots |
| `catalog.yaml` | Your own App catalog entries (optional) |
| `psccm.log` | Service log |

## First steps

1. **Settings → Credentials.** If the service already runs as a domain admin-type account, Windows
   needs no credential. Otherwise add a Windows credential (`CORP\deploy`) and tick *default for
   Windows*. For Linux add an SSH credential (user with sudo) and tick *default for Linux*. Add
   credentials **in the console**, so they are encrypted for the account that runs the service.
2. **Computers → Add computers**, or **Import from AD**. Create **groups** for the sets you deploy to.
3. **Deploy**: pick a package, pick groups, tags or computers, and press **Deploy**. Watch the live log
   for each computer.

### Network access without the installer

The console only listens on `127.0.0.1` unless a token is set:

```bash
psccm serve --bind 0.0.0.0 --port 8080 --token "long-random-string"
psccm service install --bind 0.0.0.0 --token "long-random-string" --account CORP\svc-psccm
```

For the second command, `PSCCM_SERVICE_PASSWORD` holds the account password; run it as administrator.

## Preparing the targets

**Windows** (one-time, best done by GPO):

- *Computer Configuration → Administrative Templates → Windows Components → Windows Remote Management
  → WinRM Service → Allow remote server management through WinRM* = Enabled
- Service *WinRM* set to Automatic, and the firewall allowing TCP 5985 (or 5986 for HTTPS) from the console.
- Or on a single machine: `Enable-PSRemoting -Force`.
- Use FQDNs so Kerberos works. For IP addresses or non-domain machines, add them to the console's
  `TrustedHosts` and use a credential, or switch on HTTPS in *Settings → Windows (WinRM)*.

**Linux:**

- SSH reachable on port 22 (or set a port on the computer).
- A user with sudo. With password login the same password is used for sudo. With key login, sudo
  must be `NOPASSWD` (or log in as root).
- `bash` and `curl` or `wget`. Host keys are pinned on first connect; a changed key fails the
  deployment until you *forget* it in Settings.

## Active Directory (read-only)

*Settings → Active Directory*. Two modes:

- **Account running PSCCM** (Windows, recommended): uses ADSI with the console's own Kerberos
  ticket. No password is stored, and it works with DCs that require LDAP signing.
- **LDAP with a stored credential**: any normal domain user (`reader@corp.local`), over LDAPS 636 or
  StartTLS. This also works when the console runs on Linux.

PSCCM only performs LDAP **searches** for `objectCategory=computer`. No code path writes to AD, so the
account needs no delegated rights. Options cover the OUs to search, an extra LDAP filter, skipping
disabled or stale computers, tagging by OU (`OU=Web Servers` → tag `web-servers`) and auto-sync every N
minutes. Imports add and refresh computers; they never delete any. Each computer's AD security group
memberships are imported too, so they can drive dynamic groups.

## Computer groups

*Computers → Groups*. A group is either:

- **Static**: a fixed list. Select computers and choose *Add to group*.
- **Dynamic**: every computer that matches **all** of the rules you fill in: OS, name pattern
  (`web*, app0?`), AD OU contains, OS version contains (`Server 2022`), AD security group, tag. It is
  re-evaluated whenever it is used, so new computers from AD join automatically. The editor previews
  the matching computers as you type.

Dynamic groups can also use the [inventory](#inventory): installed software with an optional version
condition (`Google Chrome*` with `<130`, or *without* `Notepad++*`), *reboot pending*, and *a disk
has less than N GB free*. These rules only match computers that have been scanned. The rule
*Windows updates missing* uses the last [update scan](#windows-update).

You can deploy and schedule to groups (and combine them with tags and single computers). CLI:
`psccm deploy 7zip --group "Web farm"`, `psccm groups list`.

## Inventory

*Inventory* page. A scan runs a read-only script on each computer over the same WinRM/SSH connection
and credential that deployments use. Nothing is installed on the targets and nothing is changed. It
collects:

| | Windows | Linux |
|---|---|---|
| OS | name, version (`23H2`), build, architecture, install date, last update installed | `/etc/os-release`, kernel, architecture, package manager |
| Hardware | manufacturer, model, serial number, CPU, logical CPUs, memory | the same, from `/sys/class/dmi` and `/proc` |
| State | last boot, logged-on user, IPv4 addresses, domain, reboot pending (CBS, Windows Update, rename) | last boot, logged-on users, IPv4 addresses, reboot pending (`/var/run/reboot-required`, `needs-restarting`) |
| Disks | fixed drives: size, free space, file system | mounted file systems (no tmpfs/overlay) |
| Software | *Programs and Features* (64- and 32-bit, no system components or updates) and Store / MSIX apps for any user or provisioned | `dpkg` or `rpm` packages |

- **Scan all computers** or *Scan now* on one computer. Set *Scan all computers every N hours* for a
  periodic scan (off by default).
- The **Software** tab lists every program with its versions and how many computers have each. From a
  program you can create a dynamic group of the computers **with** or **without** it, then add a version
  condition (`<130`, `>=22.11`, `!=24.09`, or `22` for any 22.x).
- **Uninstall from the Software tab**: tick computers in a program's list and choose *Uninstall from
  selected*. This opens *New deployment* with the *Uninstall a Windows program* package (or, for Linux,
  *Linux package* with `remove`) and the computers filled in, so you review it before anything runs.
- The **Outdated** tab compares the programs that each [App catalog](#app-catalog) entry detects on scanned
  Windows computers with the latest version. *Update N* opens a deployment of the catalog package to the
  outdated computers; *Add to catalog* downloads the installer first. A computer counts as up to date when
  any matching program has the latest version or newer, the same rule the package uses to skip computers.
- The **Changes** tab is a history of what changed between scans: software **installed**, **removed** or
  **updated** (old → new version) and changes to the OS version and build, kernel, memory, CPU, model,
  serial number or domain. Each computer's details show its changes for the last year too. History starts
  at a computer's second scan and is kept for 365 days.
- **Export CSV** on every tab downloads what the tab shows (with the search and filters applied).
  *Export every install* on the Software tab gives one row per program per computer. The files open
  directly in Excel.
- After a deployment succeeds, the computers it ran on are re-scanned (only computers that were scanned
  before), so software groups see the change. You can turn this off.
- A failed or offline scan keeps the last good inventory and shows the error.

CLI: `psccm inventory scan [names…]`, `psccm inventory show web01`,
`psccm inventory software --name "Google Chrome*"`, `psccm inventory changes [web01] --days 7`,
`psccm inventory outdated`.

## Windows Update

*Updates* page. PSCCM asks each Windows computer's own **Windows Update Agent** which updates are
missing and installs them. The updates come from the computer's usual source: its **WSUS** server when
one is set by policy, otherwise Windows Update. Nothing needs to be set up on the targets beyond WinRM.

Windows does not let a remote (WinRM) session download or install updates. PSCCM therefore runs the
update script as **SYSTEM** through a one-off scheduled task (`\PSCCM\Windows Update scan` / `… install`).
The task's output is streamed back into the console while it runs, and the task is removed afterwards.

- **Scan all computers**, or *Scan now* on one computer. Set *Scan all Windows computers every N hours*
  for a periodic scan (off by default). A scan only searches: nothing is downloaded or installed.
- The **Computers** tab shows each computer's missing updates (security/critical counted separately),
  pending restart, OS build, last successful install and update source. Click a computer to see its
  missing updates and recent install history (with failures).
- The **Missing updates** tab lists every missing update across the fleet with its KB, classification,
  MSRC severity, release date and the computers that need it. Filter by classification. Export to CSV.
- **Install**: tick updates and choose *Install selected…*, or use *Install on N computer(s)…* on the
  Computers tab. Both open *New deployment* with the **Windows Update - install updates** package and the
  computers filled in, so you review it before anything runs.
- **Decline** updates you never want (e.g. a problem KB): PSCCM never installs them, even through a
  schedule by classification, and they no longer count as missing. *Show declined* → *Undecline* undoes it.
  An update named explicitly in a deployment's `kbs` is still installed.
- **Patch Tuesday schedules**: *New patch schedule* creates a **monthly** schedule, by default
  2nd Tuesday + 2 days at 02:00. Pick groups and a batch size so web farms restart one server at a time.
- **Dynamic groups**: the rule *Windows updates missing: any / security or critical* uses the last scan,
  e.g. a group "Needs security patches" for a schedule or a report.
- After a deployment of the update package, the computers it reached are scanned again (on by default).
- *Look for updates at*: the computer's own source (WSUS if set, else Windows Update), or Windows Update
  directly, ignoring WSUS (e.g. while a WSUS server is being retired).

The **Windows Update - install updates** package (`windows-update`) has these variables:

| Variable | Default | Meaning |
|---|---|---|
| `classifications` | Security Updates, Critical Updates, Definition Updates, Update Rollups | Also: Updates, Feature Packs, Service Packs, Tools, Drivers, Upgrades. `*` = all. |
| `kbs` | (empty) | Install only these KBs (or update IDs). Empty = by classification. |
| `exclude_kbs` | (empty) | Never install these. Declined updates are always left out. |
| `reboot` | `if-required` | `never` leaves the restart to you; `always` restarts at the end. |
| `passes` | `2` | After a restart, another round installs the updates that needed it. |

Updates are downloaded and installed one at a time, so one failing update does not stop the rest. The
log shows each one. A computer is marked failed if any update failed; it still restarts first when the
updates that did install need it. Optional updates (such as feature updates offered as "download and
install now") are only installed when named in `kbs`.

CLI: `psccm updates scan [names…]`, `psccm updates list [--security]`, `psccm updates show web01`.

## Group Policy (GPO) import and injection

*Group Policy* page. PSCCM reads GPOs from AD (`groupPolicyContainer` objects and their `gPLink`
links, read-only) and copies a GPO's folder from SYSVOL into a local snapshot. It can also import a
**GPO backup folder** (GPMC *Back Up…* / `Backup-GPO`), which needs no AD access at all.

Each imported GPO becomes a package `gpo-<name>-<id>` that applies the GPO's **computer settings
directly on the computers you choose**. Nothing is linked or changed in AD. It covers:

| GPO part | How it is applied |
|---|---|
| Administrative Templates / registry policies (`Machine\Registry.pol`), including `**del.` / `**delvals.` / `**soft.` | Written to `HKLM` |
| Security settings (`GptTmpl.inf`: password/lockout policy, user rights, security options…) | `secedit /configure` with a separate database |
| Advanced audit policy (`audit.csv`) | `auditpol /restore` |
| Preferences, scripts, software installation, user settings | Listed as *not applied* |

Deploy it like any package: once, to a group, or on a schedule to keep computers compliant
(re-applying fixes drift). With the variable `mode=remove` the registry values the GPO sets are
deleted again; security and audit settings are not reverted. *Re-import* refreshes the snapshot when
the GPO changes in AD (the page shows outdated imports). CLI: `psccm gpo list`,
`psccm gpo import <GUID>`, `psccm gpo import-folder <path>`.

Reading SYSVOL uses the service account (Windows mode) or the AD credential (LDAP mode).

---

## Agent (for computers without WinRM/SSH)

Most fleets have machines with neither WinRM nor SSH enabled, so the console cannot reach them
inbound. The **PSCCM agent** solves this: it is the same `psccm` executable run as a small resident
service that **connects out** to the console over HTTPS and long-polls for work. Nothing is opened
inbound on the target. Because the agent runs as **LocalSystem**, it can do everything locally that
would otherwise need remoting.

With an agent you get, per computer:

- **Analytics** — CPU, memory, disk/HDD usage, uptime and process count, updated continuously, shown
  on the **Agents** page with live bars.
- **On-demand event logs** — collect from System/Application/Security/Setup with level, time-range and
  event-ID filters (`Get-WinEvent`).
- **Scheduled Tasks** — sync the Task Scheduler inventory and run / enable / disable / delete tasks.
- **Windows Firewall** — list rules and add / enable / disable / delete them.
- **Local users & groups** (the lusrmgr jobs) — create / delete / enable / disable users, set
  passwords, and change group membership.
- **Web terminal** — an interactive `cmd`/PowerShell (or `bash`) session in the browser, run as
  SYSTEM, as the logged-on user (no password), or as a specific account (password runas).
- **Commands** — run an ad-hoc script and see its output, optionally under a chosen account.

### Enrolling agents

*Settings → Agents* holds the shared **enrollment token** and a ready-to-run installer. There are two
ways to get the agent onto a machine:

1. **Standalone / GPO (no remoting needed).** Copy the one-line PowerShell command from
   *Settings → Agents* and run it as administrator on the target, or drop it into a **GPO startup /
   login script** or any other push tool. It downloads the agent from the console
   (`/download/agent`, gated by the enrollment token) and runs `psccm agent install`.
2. **Deploy via WinRM/SSH.** For machines that still have remoting, put the `psccm` binary in the
   repository folder (`psccm.exe` for Windows, `psccm` for Linux) and deploy the built-in
   **Install PSCCM Agent** package with the console URL and token as variables.

The agent registers as the Windows service **`PSCCMAgent`** (LocalSystem) or a Linux
`psccm-agent` systemd unit, enrolls, and then appears under **Agents**. Re-running the installer on a
known computer re-enrolls into the same record. Agents self-update from the same GitHub releases as
the console.

CLI: `psccm agent install --server https://console:8080 --token <tok>`, `psccm agent status`,
`psccm agent run` (foreground, for testing), `psccm agent uninstall`.

### Security

The agent runs as LocalSystem, so anyone who can use the console can run commands and change the
firewall, tasks and local accounts on every enrolled machine — protect the console like a domain
admin account (bind to localhost or set a strong `--token`). Agent traffic authenticates with the
enrollment token (to join) and then a per-agent key; the console accepts self-signed HTTPS. Run-as
passwords are sent to the agent over the connection and masked in the console UI; impersonating the
logged-on user needs no password and is preferred.

---

## App catalog

*App catalog* page. A list of common Windows applications that PSCCM keeps at their latest
version: 7-Zip, Google Chrome, Mozilla Firefox, Node.js LTS, Python 3, Java JDK 21 (Temurin),
Notepad++, Visual Studio Code, Git for Windows, PowerShell 7, PuTTY, KeePassXC and VLC.

- **Add** looks up the newest version at the vendor (GitHub release, JSON API or download page),
  downloads the 64-bit installer **once to the console** (`repository\catalog\<app>\<version>\`)
  and creates a package `catalog-<app>` in the *App catalog* category. Targets get the installer
  pushed over WinRM, so they need no internet access.
- The download is checked against the **SHA-256 the vendor publishes** where there is one (GitHub
  release digests, `SHA256SUMS` files, the Adoptium API); otherwise it is HTTPS-only and the hash is
  recorded. Every deployment checks the file's SHA-256 on the target before installing.
- The package's detect check skips computers that already have **that version or newer**, so it is
  safe to deploy again and again.
- **Daily check** (on by default): new versions of added apps are downloaded and their packages
  updated. The previous version is kept in the repository.
- **Auto-deploy**: pick groups per app and each new version is deployed to them as soon as it is
  downloaded. That covers "install or update it on every machine" without a schedule. For a fixed
  maintenance window, leave it empty and add a schedule for `catalog-<app>` instead.

GitHub allows 60 API requests an hour without a token, which is plenty for the daily check.

**Your own apps** go in `data\catalog.yaml`, in the same format as the built-in
[catalog/apps.yaml](catalog/apps.yaml); an entry with the same `id` replaces the built-in one:

```yaml
- id: winmerge
  name: WinMerge
  category: Utilities
  description: Compare and merge files and folders.
  version: { github: WinMerge/winmerge }               # latest GitHub release
  installer:
    asset: '^WinMerge-[\d.]+-x64-Setup\.exe$'          # release file (regex)
    args: /VERYSILENT /NORESTART /SP- /ALLUSERS
  detect: WinMerge*                                    # installed-program name
```

Other sources: `version: { url: <page or JSON>, regex: '...(\d+\.\d+)...' }` or `json: /pointer`,
and `installer: { url: https://.../{{version}}/setup.msi }` (also `{{version_nodots}}`), with
`sha256_url:` for a SHA256SUMS list. Only HTTPS sources are accepted.

CLI: `psccm catalog list`, `psccm catalog add 7zip google-chrome`, `psccm catalog update`,
`psccm catalog check`. (The CLI downloads but does not auto-deploy; the console's daily check does.)

## Microsoft 365 Apps, Teams and Store apps

- **Microsoft 365 Apps (Office)** — package `microsoft-365-apps`. Downloads the Office Deployment
  Tool (checked to be Microsoft-signed), writes a `configuration.xml` from the package settings
  (product, channel, 32/64-bit, languages, apps to leave out, shared computer activation, remove MSI
  Office) and runs `setup.exe /configure`. Use your own file from
  [config.office.com](https://config.office.com) by putting it in the repository and naming it in
  *Configuration file*. Office comes from the Microsoft CDN, or from a share made with
  `setup.exe /download` (*Source path*) to save bandwidth. Computers that already have the product
  are skipped.
- **Microsoft Teams (new)** — package `microsoft-teams`. Runs Microsoft's `teamsbootstrapper.exe -p`,
  which provisions Teams for every user (they get it at their next sign-in). Put `MSTeams-x64.msix`
  in the repository and name it in *MSIX file* to install without each computer downloading 280 MB.
- **Any Store / MSIX app** — package `install-msix`, or an `install:` step with an `.msix`,
  `.msixbundle`, `.appx` or `.appxbundle`. The package is provisioned for all users with
  `Add-AppxProvisionedPackage` (a plain `Add-AppxPackage` over WinRM would only install it for the
  deployment account). Get the files for a Store app with
  `winget download --id <id> --source msstore`. Dependencies (VCLibs, UI.Xaml) go in a `.zip`, and
  the ones for other processor types are left out.
- **Inventory and uninstall.** Scans list Store / MSIX apps installed for any user or provisioned,
  except frameworks and Windows system apps, so they work in software groups and the Software tab.
  *Uninstall from selected* (package `uninstall-app`) removes them for all users and from the
  provisioned apps. It removes Microsoft 365 Apps silently too. The first scan that reads Store
  apps does not count them as newly installed, and a scan that cannot read them keeps the previous
  ones.

Installing straight from the Microsoft Store (`winget install --source msstore`) is not used.
winget is a per-user app that is often missing from WinRM sessions, and the msstore source needs
a signed-in user to accept its terms.

---

## Deploy options

| Option | Meaning |
|---|---|
| Parallel computers | How many computers are worked on at the same time (default 10). |
| Batch size | Rolling deployment. `1` = one computer at a time (HA pairs, web farms). `0` = all. |
| Stop after failures | Don't start more computers once this many failed. `0` = stop at the first failure. |
| Skip computers that already have it | Run the package's `detect` script first; exit 0 = skip. |
| Start at | Run later (one-off). For repeats use **Schedules** (daily/weekly/monthly/interval). |

Tags are resolved when a deployment starts, so a schedule on tag `web` picks up new web servers
imported from AD.

Failed computers can be re-run with **Redeploy to failed**. Logs, the variables used and a snapshot
of the package are kept with each deployment.

## CLI

```text
psccm serve [--bind IP] [--port N] [--token T] [--no-browser]
psccm deploy <package> -t web01,web02 [--group "Web farm"] [--tag web] [-v version=22] [--batch-size 1] [--max-failures 0] [--force]
psccm run web01,lb01 -s "Get-Service W3SVC"          # ad-hoc command (PowerShell / bash)
psccm packages list | show <id> | validate <file.yaml>
psccm computers list | add <names> [--os linux] [--tag web] | remove <names> | check
psccm groups list | create <name> --computers a,b
psccm inventory scan [names…] | show <name> | software [--name "Chrome*"]
psccm updates scan [names…] | list [--security] | show <name>
psccm ad sync [--import]
psccm gpo list | import <GUID> | import-folder <path> [--name N]
psccm catalog list | check | add <ids> | update [ids] | remove <id>
psccm cred add --name deploy --kind ssh --user deploy [--key ~/.ssh/id_ed25519] [--default-for linux]
psccm update [--install]
psccm service install [--bind IP] [--port N] [--token T] [--account DOMAIN\user] | uninstall
psccm history
```

`deploy` and `run` wait for the result, print every computer's log and exit with `1` if any
computer failed, so they can be used from CI/CD. Use `--data-dir` (or `PSCCM_DATA`) to point at the
console's data folder.

---

## Writing packages

Packages are YAML files in `data\packages\`. Edit them in the console (*Packages → package → Definition*),
which validates before saving, or with any editor. `psccm packages validate file.yaml` checks a file.

```yaml
id: my-app                 # a-z 0-9 - _
name: My App
version: "2.1"
category: Custom
description: Install My App and restart its service.
platforms: [windows]       # windows, linux or both (empty = both)

variables:
  version: { default: "2.1", description: Version to install }
  license: { required: true, secret: true }      # secret = masked in logs/UI
  edition: { default: std, options: [std, pro] } # dropdown

detect:                    # exit 0 = already installed -> computer is skipped
  windows: |
    if (Test-AppInstalled -Name 'My App*' -MinVersion '{{version}}') { exit 0 }
    exit 1

steps:
  - name: Install My App {{version}}
    install:
      file: apps/myapp-{{version}}.msi   # from data\repository (or url: https://...)
      args: LICENSE={{license}} EDITION={{edition}}
      sha256: 9f86d0...                  # optional integrity check

  - name: Copy config
    copy: { src: "apps/myapp.config", dest: 'C:\Program Files\MyApp\myapp.config' }

  - name: Restart service
    service: { name: MyAppSvc, action: restart }   # start | stop | restart

  - name: Custom script
    script: |
      Write-Output "Hello from {{host}}"
    shell: powershell      # powershell | cmd (Windows), bash | sh (Linux); default by OS

  - name: Only on Server 2016
    when: |                # step runs only if this exits 0
      if ((Get-CimInstance Win32_OperatingSystem).Caption -like '*2016*') { exit 0 }; exit 1
    script: ...

  - name: Reboot
    reboot: true           # reboots and waits until the computer is back

  - name: Pause
    wait: 30               # seconds

  - name: Patch
    windows_update:        # install missing Windows updates (see Windows Update)
      classifications: Security Updates, Critical Updates
      exclude: KB5034441
      reboot: if-required  # never | always

  - name: Shared steps
    include: other-package # run another package's steps (nested package)

  - name: Drain on load balancer
    delegate_to: "{{haproxy_host}}"   # run this step on another computer
    os: linux
    script: echo "set server web/{{host_short}} state drain" | socat stdio /run/haproxy/admin.sock

verify:                    # after all steps; non-zero = failed
  windows: if (Test-AppInstalled -Name 'My App*' -MinVersion '{{version}}') { exit 0 }; exit 1

rollback:                  # runs (best effort) when a step or verify fails
  - name: Restore backup
    script: ...
```

**Every step** also accepts `os`, `when`, `timeout` (seconds, default 1800; per install round for
`windows_update`, default 4 hours), `retries`,
`retry_delay` (default 10), `success_codes` (default `[0]`; install steps also accept `3010` and `1641`),
`continue_on_error` and `delegate_to`.

**Install** on Windows: `.msi` runs `msiexec /i … /qn /norestart /l*v` (the MSI log tail is shown on
failure), `.msp` runs `msiexec /p`, `.msu` runs `wusa /quiet /norestart`, `.msix`/`.msixbundle`/`.appx`/
`.appxbundle` are provisioned for all users (`args`: dependency packages, folders or `.zip`s and an
optional license `.xml`, separated by `;`), and anything else is executed with `args`. On Linux: `.deb` uses `apt-get install`, `.rpm` uses `dnf/yum install`, and `.sh/.run`
runs with bash.

**Built-in variables:** `{{host}}`, `{{host_short}}` (first DNS label), `{{os}}`, `{{workdir}}` (a temp
folder on the target, removed afterwards) and `{{deployment_id}}`. Variable defaults may use them
(e.g. `default: "{{host_short}}"`).

**Helpers available in every script.**

PowerShell:

- `Get-InstalledApp -Name 'Chrome*'` (registry programs and Store / MSIX apps; `-NoStore` for the registry only)
- `Get-InstalledAppx -Name 'MSTeams'` (Store / MSIX apps by display or package name)
- `Test-AppInstalled -Name 'X*' -MinVersion 1.2`
- `ConvertTo-PsccmVersion`
- `Get-PsccmFile -Url -OutFile`
- `Invoke-PsccmProcess -FilePath -Arguments` (returns the exit code)
- `Install-PsccmAppx -Path x.msix -Arguments 'deps.zip'` (provision for all users; returns 0 or 1)
- `Uninstall-App -Name 'X*'`

Scripts run with `$ErrorActionPreference = 'Stop'`, and the last native exit code is the step result.

bash (runs with `set -eo pipefail`):

- `pkg_manager`, `pkg_install pkgs…`, `pkg_installed pkg`
- `version_ge A B`
- `download URL FILE`
- `backup_file FILE`

## Typical jobs

| Job | Package | Tip |
|---|---|---|
| Common apps, always current | *App catalog* → Add, then Auto-deploy to a group | New versions are downloaded daily and rolled out; PCs that have it are skipped. |
| New utility on all PCs | `7zip`, `google-chrome`, `notepad-plus-plus`, `install-msi`, `install-exe`, `install-msix` | Target an AD tag; detect skips machines that already have it. |
| Office on new PCs | `microsoft-365-apps` | *Source path* on a local share saves each PC downloading ~3 GB. |
| Move to the new Teams | `microsoft-teams`, then `uninstall-app` with `Teams Machine-Wide Installer` | Users get the new Teams at their next sign-in. |
| Node.js upgrade | `nodejs` (`version` = `lts`, `22`, or `22.11.0`) | Set `restart_services` to restart your Node services. |
| IIS server build | `iis-install` | `reboot_if_needed: yes` to finish in one go. |
| Website release | `iis-site-deploy` | Zip in `repository\web\`; backups kept next to the site folder. |
| Zero-downtime web farm release | `iis-rolling-deploy` | Batch size 1, Stop after failures 0. |
| HAProxy upgrade | `haproxy-upgrade` | Batch size 1 on HA pairs; config backup restored on failure. |
| HAProxy config change | `haproxy-config` | Validated with `haproxy -c` before it goes live. |
| Patch Windows | `windows-update` | *Updates* → *New patch schedule* (Patch Tuesday + 2), batch size for farms. |
| Patch Linux | `linux-update` | Use a weekly schedule. |

## Security notes

- The console binds to localhost unless a token is set. Anyone who can use the console can run commands
  as admin on every managed computer, so protect it like a domain admin account.
- Passwords are encrypted with DPAPI (Windows) and never returned by the API; secret variables are
  masked in logs. On a Linux console, secrets are only base64-encoded, so prefer SSH keys there and keep
  `data/` at mode 700.
- WinRM over HTTP in a domain is encrypted by Kerberos. Use HTTPS for workgroup machines.
- SSH host keys are pinned on first use (TOFU).
- Updates are only installed when the download matches the release's `SHA256SUMS`.

## Updates

*Settings → Updates*. PSCCM checks the GitHub releases of `parthh37/psccm` once a day (configurable)
and shows a notice in the sidebar when a newer version exists. **Install** downloads the new release
and verifies its SHA-256. On Windows it then runs the setup silently: the setup stops the service,
replaces the files and starts it again with the same account and settings. On Linux it replaces the
binary and exits, so systemd restarts it. *Install updates automatically* does this unattended, but
never while a deployment is running. CLI: `psccm update [--install]`.

## Releases and release tests

Push a tag `vX.Y.Z` (matching `Cargo.toml`). The [Release workflow](.github/workflows/release.yml):

1. runs the tests and builds `psccm.exe`, the Inno Setup installer and the Linux binary;
2. publishes a GitHub release with `SHA256SUMS`;
3. **tests the published release** on fresh CI machines with
   [tests/release-test.ps1](tests/release-test.ps1):
   - **Windows:** silent install, service, upgrade in place, real WinRM deployments to the runner,
     every step type, rollback, GPO apply/remove, 7-Zip, Node.js, IIS install + site deploy +
     health check, and uninstall.
   - **Linux:** SSH + sudo deployments, every step type, 7-Zip, Node.js, HAProxy install, config
     deploy, rejected bad config with rollback, and drain/ready.
4. attaches `test-report-windows.html` and `test-report-linux.html` to the release.

Run the same tests yourself against test machines:

```powershell
./tests/release-test.ps1 -Setup .\psccm-setup-0.2.0.exe -WindowsTarget testsrv01 -RealInstalls
pwsh ./tests/release-test.ps1 -Exe ./psccm-x86_64-linux -LinuxTarget 10.0.0.5 -LinuxUser deploy -LinuxPassword '...'
```

Results go to `test-report/report.html` (also `report.json` and `junit.xml`). `-RealInstalls`
installs software and changes the targets, so use it on test machines only.

## Development

```bash
cargo test        # unit tests, incl. every built-in script parsed by PowerShell and bash
cargo run -- serve
./tests/release-test.ps1 -Exe target/release/psccm.exe   # API/CLI tests without targets
```

In debug builds, setting `PSCCM_WINRM_LOCAL=1` runs "Windows" targets on the console itself without
WinRM, which is useful to test packages on a dev box.

Source map: `src/package.rs` (YAML format), `src/engine.rs` (deployment runner), `src/catalog.rs` (App catalog),
`src/transport/` (WinRM, SSH), `src/ad.rs`, `src/gpo.rs`, `src/groups.rs`, `src/inventory.rs`
(scan scripts in `src/assets/`), `src/update.rs`, `src/service.rs`, `src/scheduler.rs`,
`src/web.rs` (API), `src/agent/` (agent protocol, runtime, jobs, Win32 run-as and the web
terminal), `web/` (UI), `installer/` (Inno Setup),
`packages/` (built-ins).
