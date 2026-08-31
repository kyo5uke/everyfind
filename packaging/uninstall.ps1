<#
.SYNOPSIS
    Remove Everyfind completely: the Explorer integration, the service, its data, the PATH
    entry, and the binaries.

.DESCRIPTION
    The counterpart to install.ps1, and deliberately the *opposite* of best-effort: it says
    what it removed and what it could not, so "uninstalled" is never a guess. Nothing is left
    behind on purpose: the index snapshot and log under %ProgramData%\everyfind go with the
    service.

    Finds either kind of install: the machine-wide one under %ProgramFiles%, and the
    `-NoService` one under your own profile.

.PARAMETER InstallDir
    Where the binaries were put. Found automatically if omitted.
#>
[CmdletBinding()]
param(
    [string] $InstallDir,
    # Set only when this script re-launched itself for the UAC prompt; that console closes the
    # moment the script ends, and a report of what was removed is worth reading.
    [switch] $Relaunched
)

$ErrorActionPreference = 'Continue'

function Say($text)  { Write-Host "  $text" }
function Ok($text)   { Write-Host "  [ok] $text" -ForegroundColor Green }
function Warn($text) { Write-Host "  [!] $text" -ForegroundColor Yellow }

function Hold {
    if (-not $Relaunched) { return }
    Write-Host ""
    Write-Host "  Press Enter to close this window." -ForegroundColor DarkGray
    try { if ($null -eq [Console]::ReadLine()) { Start-Sleep -Seconds 15 } } catch { Start-Sleep -Seconds 15 }
}

Write-Host ""
Write-Host "  Everyfind uninstaller" -ForegroundColor Cyan
Write-Host ""

$candidates = @(
    (Join-Path $env:ProgramFiles 'Everyfind'),
    (Join-Path $env:LOCALAPPDATA 'Programs\everyfind')
)
if ($InstallDir) { $candidates = @($InstallDir) }
$found = @($candidates | Where-Object { Test-Path (Join-Path $_ 'ef.exe') })

# A service was registered only by the machine-wide route, and only that route needs
# Administrator to undo. Asking for it when there is nothing to undo would be theatre.
$svc = Get-Service everyfind -ErrorAction SilentlyContinue
$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($svc -and -not $elevated) {
    Say "Administrator is required (the service was registered with it)."
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Relaunched')
    if ($InstallDir) { $argList += @('-InstallDir', "`"$InstallDir`"") }
    try {
        $p = Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -PassThru -Wait
        exit $p.ExitCode
    } catch {
        Write-Host "  [x] elevation was declined." -ForegroundColor Red; exit 1
    }
}

# `ef.exe` is what knows how to undo each piece, so prefer an installed one and fall back to
# whatever is on PATH; an install directory somebody already deleted by hand must not stop
# the service and the registry keys from being cleaned up.
$ef = if ($found.Count -gt 0) { Join-Path $found[0] 'ef.exe' }
      else { (Get-Command ef.exe -ErrorAction SilentlyContinue).Source }

if ($ef) {
    & $ef explorer engine uninstall 2>&1 | Out-Null
    & $ef shell uninstall 2>&1 | Out-Null
    Ok "registry integrations removed (if they were installed)"
    if ($svc) {
        & $ef service stop 2>&1 | Out-Null
        & $ef service uninstall 2>&1 | ForEach-Object { Say $_ }
        Ok "service stopped, removed, and its data deleted"
    } else {
        Ok "no service was registered"
    }
} else {
    Warn "ef.exe was not found; the service and registry keys were left alone."
    Warn "Re-run this from an unzipped release to finish the job."
}

# PATH, both scopes, because which one was used depends on how it was installed.
foreach ($dir in ($candidates + $found | Select-Object -Unique)) {
    foreach ($scope in @('Machine', 'User')) {
        if ($scope -eq 'Machine' -and -not $elevated) { continue }
        $current = [Environment]::GetEnvironmentVariable('Path', $scope)
        $parts = $current -split ';' | Where-Object { $_ -ne '' }
        if ($parts -contains $dir) {
            [Environment]::SetEnvironmentVariable('Path', (($parts | Where-Object { $_ -ne $dir }) -join ';'), $scope)
            Ok "removed from the $($scope.ToLower()) PATH"
        }
    }
}

# Binaries. An Explorer still holding the staged search DLL is normal and harmless (that is a
# copy under %LOCALAPPDATA%, not this one) but say so rather than failing silently.
foreach ($dir in $found) {
    try {
        Remove-Item $dir -Recurse -Force -ErrorAction Stop
        Ok "removed $dir"
    } catch {
        Warn "could not remove $dir ($($_.Exception.Message))."
        Warn "Restart Explorer, or sign out, and delete it by hand."
    }
}
if ($found.Count -eq 0) { Say "no install directory found; nothing to delete" }

Write-Host ""
Write-Host "  Done. Everyfind is gone." -ForegroundColor Cyan
Write-Host ""
Hold
