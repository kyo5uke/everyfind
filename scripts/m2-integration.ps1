<#
.SYNOPSIS
    M2 real-hardware integration: drive bulk create/delete/rename + a directory rename on a
    mounted NTFS fixture, then verify (and time) that ef-index reflects them via snapshot
    resume + USN catch-up. Also covers the restart scenario (load a pre-change snapshot and
    tail only the delta).

.DESCRIPTION
    Assumes the fixture volume is already mounted (scripts\make-test-volume.ps1 -MountLetter T)
    with the default nested tree (T:\dir000\level2\level3\file_000000.txt ...). Elevated.

.PARAMETER Mount
    Fixture drive letter (default T).
.PARAMETER Ef
    Path to the release ef-index.exe.
#>
[CmdletBinding()]
param(
    [string]$Mount = 'T',
    [string]$Ef = '.\target\release\ef-index.exe'
)
$ErrorActionPreference = 'Stop'
$root = "${Mount}:"
$pre = Join-Path $env:TEMP 'ef-m2-pre.snap'

function Get-MatchCount([string]$query, [string]$snapshot) {
    # Run ef-index resuming from $snapshot, return the match count for $query.
    $out = & $Ef --volume "${Mount}:" --load-snapshot $snapshot --query $query --limit 1 2>$null
    $line = $out | Where-Object { $_ -match 'match\(es\)' }
    if ($line -match ':\s+(\d+) match') { return [int]$Matches[1] } else { return -1 }
}

Write-Host "== M2 integration on ${Mount}: =="

# Baseline snapshot BEFORE any change (captures the resume cursor).
& $Ef --volume "${Mount}:" --save-snapshot $pre 2>$null | Out-Null
Write-Host "baseline snapshot saved: $pre"

# --- Bulk operations via .NET (fast; this is test setup, not the measured path) ---
New-Item -ItemType Directory -Path "$root\newfiles" -Force | Out-Null
$gen = [Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt 4000; $i++) { [IO.File]::WriteAllText("$root\newfiles\new_$i.txt", 'x') }
for ($i = 0; $i -lt 3000; $i++) {
    $d = [int][math]::Floor($i / 500)
    [IO.File]::Delete(("{0}\dir{1:D3}\level2\level3\file_{2:D6}.txt" -f $root, $d, $i))
}
for ($i = 5000; $i -lt 8000; $i++) {
    $d = [int][math]::Floor($i / 500)
    $from = "{0}\dir{1:D3}\level2\level3\file_{2:D6}.txt" -f $root, $d, $i
    $to = "{0}\dir{1:D3}\level2\level3\ren_{2}.txt" -f $root, $d, $i
    [IO.File]::Move($from, $to)
}
# A directory rename: dir020 -> renamed_dir020 (its files must follow).
$dir20 = "$root\dir020"
if (Test-Path $dir20) { [IO.Directory]::Move($dir20, "$root\renamed_dir020") }
$gen.Stop()
Write-Host ("generated 10000 file ops + 1 dir rename in {0:N2}s" -f $gen.Elapsed.TotalSeconds)

# Let NTFS flush async delete (\$Extend\$Deleted -> FILE_DELETE) records.
Start-Sleep -Seconds 2

# --- Measured path: resume the baseline snapshot and catch up the journal delta ---
$apply = Measure-Command {
    & $Ef --volume "${Mount}:" --load-snapshot $pre --stats 2>&1 | Out-Null
}
Write-Host ("catch-up (load+apply ~10k ops): {0:N0} ms" -f $apply.TotalMilliseconds)

# --- Verify the mutations are reflected in search ---
$created = Get-MatchCount 'new_' $pre        # expect 4000
$renamed = Get-MatchCount 'ren_' $pre        # expect 3000
$deleted = Get-MatchCount 'file_000100' $pre # expect 0 (deleted)
$dirmoved = Get-MatchCount 'renamed_dir020' $pre # expect >=1

Write-Host "created (new_)      = $created  (expect 4000)"
Write-Host "renamed (ren_)      = $renamed  (expect 3000)"
Write-Host "deleted (file_000100) = $deleted  (expect 0)"
Write-Host "dir-rename entry    = $dirmoved  (expect >= 1)"

# Descendant-path proof: a file that was under dir020 must now resolve under renamed_dir020.
$child = & $Ef --volume "${Mount}:" --load-snapshot $pre --query file_010000 --limit 3 2>$null
Write-Host "sample descendant path(s) after dir rename:"
$child | Where-Object { $_ -match 'renamed_dir020|dir020' } | ForEach-Object { Write-Host "  $_" }

$ok = ($created -eq 4000) -and ($renamed -eq 3000) -and ($deleted -eq 0) -and ($dirmoved -ge 1)
Write-Host ("RESULT: {0}" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })))
