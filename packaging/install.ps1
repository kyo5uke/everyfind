<#
.SYNOPSIS
    Install everyfind: copy the binaries, and (by default) register the indexing service.

.DESCRIPTION
    Everything the README asks for by hand, in one command.

    There are two ways to let Everyfind read NTFS, and this is the only real decision in the
    install: reading the raw volume needs Administrator, so *something* has to hold that
    right:

      (default)   Register a service. It runs as LocalSystem, so from then on `ef` answers
                  from an ordinary terminal with no prompt. Asks for Administrator once.

      -NoService  Change nothing about the system. The binaries go under your own profile,
                  your own PATH picks them up, and you start the indexer yourself when you
                  want it (`efd --foreground`, elevated). No Administrator at install time,
                  no service, nothing running when you are not using it.

.PARAMETER InstallDir
    Where to put the binaries. Defaults to %ProgramFiles%\Everyfind with a service (the
    service runs as LocalSystem, so the files must live somewhere it can always read), and to
    %LOCALAPPDATA%\Programs\everyfind with -NoService (no Administrator needed).

.PARAMETER Volume
    The NTFS volume to index. Default: C:

.PARAMETER NoService
    Do not register a service, and make no machine-wide change at all.

.PARAMETER WithExplorer
    Also put Everyfind behind Explorer's own search box. Off by default: it writes three
    per-user COM registration keys, and that is a decision to make deliberately rather than
    have made for you. Undo any time with `ef explorer engine uninstall`.

.PARAMETER NoPath
    Do not touch PATH.

.EXAMPLE
    .\install.ps1
.EXAMPLE
    .\install.ps1 -NoService
.EXAMPLE
    .\install.ps1 -WithExplorer -Volume D:
#>
[CmdletBinding()]
param(
    [string] $InstallDir,
    [string] $Volume = 'C:',
    [switch] $NoService,
    [switch] $WithExplorer,
    [switch] $NoPath,
    # Set only when this script re-launched itself for the UAC prompt. That console is not the
    # one anybody typed in, and it closes the instant the script ends, taking everything it
    # printed with it - including the reason an install failed.
    [switch] $Relaunched
)

$ErrorActionPreference = 'Stop'

# Whether the running service was stopped to make way for new binaries. If it was, every exit
# path owes it a restart: the alternative is a machine left with no index and nothing said.
$script:stopped = $false

function Say($text)  { Write-Host "  $text" }
function Ok($text)   { Write-Host "  [ok] $text" -ForegroundColor Green }
function Warn($text) { Write-Host "  [!] $text" -ForegroundColor Yellow }

function Recover {
    if (-not $script:stopped) { return }
    if (-not (Get-Service everyfind -ErrorAction SilentlyContinue)) { return }
    $ef = Join-Path $InstallDir 'ef.exe'
    if (-not (Test-Path $ef)) { return }
    Warn "putting the service that was stopped for this upgrade back the way it was..."
    try { & $ef service start 2>&1 | Out-Null } catch { }
}

function Hold {
    if (-not $Relaunched) { return }
    Write-Host ""
    Write-Host "  Press Enter to close this window." -ForegroundColor DarkGray
    # No stdin (a scripted run) means ReadLine returns at once; a short pause is still better
    # than a window that vanishes before it can be read.
    try { if ($null -eq [Console]::ReadLine()) { Start-Sleep -Seconds 15 } } catch { Start-Sleep -Seconds 15 }
}

function Die($text)  { Write-Host "  [x] $text" -ForegroundColor Red; Recover; Hold; exit 1 }

# Anything that throws on its own - a locked file, a denied write - would otherwise end the
# script without passing through Die, which is how an interrupted upgrade used to leave the
# service stopped, unexplained, in a console that had already closed.
trap {
    Write-Host "  [x] $($_.Exception.Message)" -ForegroundColor Red
    Recover
    Hold
    exit 1
}

Write-Host ""
Write-Host "  Everyfind installer" -ForegroundColor Cyan
Write-Host ""

# Resolved here rather than in `param`, because the right default depends on -NoService: a
# service reads the binaries as LocalSystem and needs them somewhere machine-wide, and an
# install that promised to change nothing must not write to %ProgramFiles%.
if (-not $InstallDir) {
    $InstallDir = if ($NoService) { Join-Path $env:LOCALAPPDATA 'Programs\everyfind' }
                  else            { Join-Path $env:ProgramFiles 'Everyfind' }
}

$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# --- Administrator, only when the chosen route actually needs it ------------------------
# Re-launch elevated rather than failing halfway through: the file copy would succeed and the
# service registration would not, which is the one outcome worse than not starting at all.
if (-not $NoService -and -not $elevated) {
    Say "Administrator is required to register the indexing service."
    Say "(Run with -NoService to install without it, and without any system change.)"
    Say "Re-launching with a UAC prompt..."
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
                 '-InstallDir', "`"$InstallDir`"", '-Volume', $Volume, '-Relaunched')
    if ($WithExplorer) { $argList += '-WithExplorer' }
    if ($NoPath)       { $argList += '-NoPath' }
    try {
        $p = Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -PassThru -Wait
        exit $p.ExitCode
    } catch {
        Die "elevation was declined. Re-run from an Administrator terminal, or use -NoService."
    }
}

# --- Preflight -------------------------------------------------------------------------
# Checked before anything is written, so a machine that cannot run Everyfind is told why
# instead of ending up half-installed.
if (-not [Environment]::Is64BitOperatingSystem) { Die "Everyfind is 64-bit only." }

$osv = [Environment]::OSVersion.Version
if ($osv.Major -lt 10) { Die "Windows 10 or later is required (found $osv)." }

$src = $PSScriptRoot
foreach ($f in @('ef.exe', 'efd.exe')) {
    if (-not (Test-Path (Join-Path $src $f))) {
        Die "$f is not next to this script. Run it from the unzipped release."
    }
}

$letter = $Volume.TrimEnd('\', ':')
$disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$letter`:'" -ErrorAction SilentlyContinue
if (-not $disk)                  { Die "drive $letter`: was not found." }
if ($disk.FileSystem -ne 'NTFS') { Die "drive $letter`: is $($disk.FileSystem); Everyfind reads NTFS structures and needs NTFS." }
Ok "Windows $osv / x64 / NTFS ($letter`:)"

# --- Files -----------------------------------------------------------------------------
# Stop a running service first: its own exe cannot be replaced while it is running, and that
# failure ("being used by another process") arrives after some files have already been copied.
$svc = Get-Service everyfind -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped' -and (Test-Path (Join-Path $InstallDir 'ef.exe'))) {
    Say "stopping the running service to replace its binaries..."
    & (Join-Path $InstallDir 'ef.exe') service stop 2>&1 | Out-Null
    $script:stopped = $true
    Start-Sleep -Milliseconds 500
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$copied = 0
foreach ($f in @('ef.exe', 'efd.exe', 'ef-index.exe', 'ef_search_engine.dll',
                 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD-PARTY-NOTICES.txt')) {
    $p = Join-Path $src $f
    if (-not (Test-Path $p)) { continue }
    # An `ef` still open in another terminal holds ef.exe, and this is the first step that can
    # fail after the service has already been stopped.
    try { Copy-Item $p $InstallDir -Force -ErrorAction Stop; $copied++ }
    catch { Die "could not replace $f ($($_.Exception.Message)). Close any running 'ef' and run this again." }
}
Ok "$copied file(s) -> $InstallDir"

$ef = Join-Path $InstallDir 'ef.exe'

# --- PATH ------------------------------------------------------------------------------
# Machine-wide only when this install is machine-wide anyway. -NoService promises to leave the
# system alone, and the user's own PATH is not the system's.
if (-not $NoPath) {
    $scope = if ($NoService) { 'User' } else { 'Machine' }
    $current = [Environment]::GetEnvironmentVariable('Path', $scope)
    $parts = $current -split ';' | Where-Object { $_ -ne '' }
    if ($parts -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $InstallDir) -join ';'), $scope)
        Ok "added to the $($scope.ToLower()) PATH (open a new terminal to pick it up)"
    } else {
        Ok "already on the $($scope.ToLower()) PATH"
    }
    $env:Path = "$env:Path;$InstallDir"
}

# --- The indexer -----------------------------------------------------------------------
if ($NoService) {
    Ok "no service registered, and nothing machine-wide was changed"
    Write-Host ""
    Write-Host "  Start the indexer when you want it, from an Administrator terminal:" -ForegroundColor Cyan
    Write-Host "      efd --foreground --volume $letter`:"
    Write-Host ""
    Write-Host "  Leave that running, and from any ordinary terminal:" -ForegroundColor Cyan
} else {
    if (Get-Service everyfind -ErrorAction SilentlyContinue) {
        Say "the service is already registered; re-registering it against these binaries..."
        & $ef service uninstall 2>&1 | Out-Null
    }
    & $ef service install --volume "$letter`:" 2>&1 | ForEach-Object { Say $_ }
    if ($LASTEXITCODE -ne 0) { Die "service registration failed." }
    Ok "service registered (volume $letter`:)"

    & $ef service start 2>&1 | ForEach-Object { Say $_ }
    if ($LASTEXITCODE -ne 0) { Die "the service did not start." }
    Ok "service started"

    # The first start enumerates the whole volume. Waiting here (and showing the count
    # climbing) is the difference between "installed" and "installed and actually usable": a
    # search run a second later would otherwise answer "still building".
    # 30s warm, 173s measured on the same volume with a cold cache: the pass reads the whole
    # MFT, so promising the warm number to somebody installing for the first time - who has the
    # cold one by definition - is how a working install starts to look like a hung one.
    Say "building the first index - the one slow step, 30s to a few minutes for ~6M files..."
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $entries = 0
    while ($sw.Elapsed.TotalMinutes -lt 10) {
        Start-Sleep -Seconds 2
        $status = (& $ef status 2>&1) -join "`n"
        # While the index is building, the number that moves is on the `state` line ("... (N
        # entries so far)"); the `entries` line reads 0 until the finished index is swapped in.
        # Matching the latter first is why this counter used to sit at 0 for the whole build.
        if ($status -match 'building initial index \(([\d,]+) entries') { $entries = $Matches[1] }
        elseif ($status -match 'entries\s*:\s*([\d,]+)') { $entries = $Matches[1] }
        if ($status -notmatch 'building') { break }
        Write-Host "`r    $entries so far ($([int]$sw.Elapsed.TotalSeconds)s)   " -NoNewline
    }
    Write-Host "`r                                             `r" -NoNewline
    if ($entries -eq 0) { Warn "the index did not report entries; check 'ef status'." }
    else { Ok "$entries entries indexed in $([int]$sw.Elapsed.TotalSeconds)s" }

    Write-Host ""
    Write-Host "  Done. Try it from a NEW terminal (no Administrator needed):" -ForegroundColor Cyan
}

# --- Explorer integration (opt-in) ------------------------------------------------------
if ($WithExplorer) {
    & $ef explorer engine install 2>&1 | ForEach-Object { Say $_ }
    if ($LASTEXITCODE -ne 0) { Warn "Explorer integration failed; everything else is installed." }
    else { Ok "Explorer's search box now answers with Everyfind" }
}

Write-Host "      ef readme          " -NoNewline; Write-Host "# one-shot search" -ForegroundColor DarkGray
Write-Host "      ef                 " -NoNewline; Write-Host "# interactive" -ForegroundColor DarkGray
Write-Host "      ef du              " -NoNewline; Write-Host "# what is taking up the disk" -ForegroundColor DarkGray
Write-Host "      ef status          " -NoNewline; Write-Host "# daemon health" -ForegroundColor DarkGray
if (-not $WithExplorer) {
    Write-Host ""
    Write-Host "  Optional: put Everyfind behind Explorer's own search box." -ForegroundColor DarkGray
    Write-Host "      ef explorer engine install    " -NoNewline
    Write-Host "# writes 3 per-user COM keys; undo with 'uninstall'" -ForegroundColor DarkGray
}
Write-Host ""
Write-Host "  Remove everything:  .\uninstall.ps1" -ForegroundColor DarkGray
Write-Host ""
Hold
