<#
.SYNOPSIS
    Create (or tear down) a small NTFS VHDX test volume for everyfind.

.DESCRIPTION
    Builds an NTFS-formatted VHDX with a tree of test files, mounted as a drive
    letter, for real-ish index/enumeration tests.

    Uses `diskpart` (present on every Windows edition), NOT `New-VHD`, which lives
    in the Hyper-V PowerShell module and is unavailable on Windows 11 Home.

    REQUIRES AN ELEVATED (Administrator) TERMINAL: creating/attaching a VHDX and
    creating a USN journal both need admin rights.

.PARAMETER VhdxPath
    Path of the VHDX file to create/attach. Default: repo-root\test-volume.vhdx.

.PARAMETER SizeMB
    Maximum (dynamically-expanding) size in MB. Default 512.

.PARAMETER FileCount
    Number of test files to create. Default 10000.

.PARAMETER MountLetter
    Drive letter to assign (no colon). Default T.

.PARAMETER JournalMaxBytes
    USN journal maximum size (fsutil `m=`). Default 32 MiB.

.PARAMETER JournalDeltaBytes
    USN journal allocation delta (fsutil `a=`). Default 8 MiB.

.PARAMETER TinyJournal
    Create a deliberately small USN journal (m=512 KiB, a=128 KiB) so M2 overflow /
    ERROR_JOURNAL_ENTRY_DELETED recovery can be exercised reliably. fsutil may round
    these up to its own minimum; the point is "as small as the OS allows".

.PARAMETER Detach
    Detach the VHDX instead of creating it (leaves the file on disk).

.EXAMPLE
    # Create the fixture (elevated):
    powershell -ExecutionPolicy Bypass -File scripts\make-test-volume.ps1

.EXAMPLE
    # Tear it down when done (elevated):
    powershell -ExecutionPolicy Bypass -File scripts\make-test-volume.ps1 -Detach
#>
[CmdletBinding()]
param(
    [string]$VhdxPath = '',
    [int]$SizeMB = 512,
    [int]$FileCount = 10000,
    [ValidatePattern('^[A-Za-z]$')]
    [string]$MountLetter = 'T',
    [long]$JournalMaxBytes = 33554432,
    [long]$JournalDeltaBytes = 8388608,
    [int]$ClusterBytes = 4096,
    [switch]$TinyJournal,
    [switch]$DuFixture,
    [switch]$Detach
)

$ErrorActionPreference = 'Stop'

# Resolve the default VHDX path here (not in the param block, where $PSScriptRoot can be
# empty depending on how the script is invoked).
if (-not $VhdxPath) {
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
    $VhdxPath = Join-Path (Split-Path $scriptDir -Parent) 'test-volume.vhdx'
}

function Assert-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($id)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'This script must be run from an ELEVATED (Administrator) terminal.'
    }
}

function Invoke-Diskpart([string[]]$Lines) {
    # diskpart only accepts a script file (`/s`); it cannot read from a pipe reliably.
    $tmp = [IO.Path]::GetTempFileName()
    try {
        Set-Content -LiteralPath $tmp -Value $Lines -Encoding Ascii
        & diskpart.exe /s $tmp
        if ($LASTEXITCODE -ne 0) { throw "diskpart failed (exit $LASTEXITCODE)." }
    }
    finally {
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    }
}

Assert-Admin

if ($Detach) {
    Write-Host "Detaching VHDX: $VhdxPath"
    Invoke-Diskpart @(
        "select vdisk file=`"$VhdxPath`""
        'detach vdisk'
    )
    Write-Host 'Detached. The .vhdx file is left on disk (delete it manually if desired).'
    return
}

if (Test-Path -LiteralPath $VhdxPath) {
    throw "VHDX already exists: $VhdxPath. Remove it or run with -Detach first."
}

$letter = $MountLetter.ToUpper()

Write-Host "Creating $SizeMB MB NTFS VHDX at $VhdxPath, mounting as ${letter}: ..."
Invoke-Diskpart @(
    "create vdisk file=`"$VhdxPath`" maximum=$SizeMB type=expandable"
    'attach vdisk'
    'create partition primary'
    # Pin the cluster size so du allocated-size expectations are predictable (M5 P19/P20).
    # 4096 is the NTFS default for <16 TiB volumes, so existing M2/M4 fixtures are unaffected.
    "format fs=ntfs unit=$ClusterBytes quick label=EFTEST"
    "assign letter=$letter"
)

$root = "${letter}:\"
if (-not (Test-Path -LiteralPath $root)) {
    throw "Volume ${letter}: did not come online after attach."
}

# Enable a USN journal so probes P1 (QUERY_USN_JOURNAL) can be exercised here too.
# New NTFS volumes have no journal until one is created; this write FSCTL is allowed
# ONLY on test volumes.
$jMax = if ($TinyJournal) { 524288 } else { $JournalMaxBytes }
$jDelta = if ($TinyJournal) { 131072 } else { $JournalDeltaBytes }
Write-Host "Creating USN journal (m=$jMax a=$jDelta) on ${letter}: ..."
& fsutil.exe usn createjournal "m=$jMax" "a=$jDelta" "${letter}:" | Out-Null

Write-Host "Generating $FileCount files under $root ..."
# Spread files across a nested tree so path reconstruction has depth to exercise.
$perDir = 500
$made = 0
$dirIdx = 0
while ($made -lt $FileCount) {
    $sub = Join-Path $root ("dir{0:D3}\level2\level3" -f $dirIdx)
    New-Item -ItemType Directory -Path $sub -Force | Out-Null
    for ($i = 0; $i -lt $perDir -and $made -lt $FileCount; $i++) {
        $f = Join-Path $sub ("file_{0:D6}.txt" -f $made)
        New-Item -ItemType File -Path $f -Force | Out-Null
        $made++
    }
    $dirIdx++
}

# A hardlink and a directory symlink, to exercise P8/P9 handling on the fixture.
$hlTarget = Join-Path $root 'dir000\level2\level3\file_000000.txt'
$hlLink   = Join-Path $root 'hardlink_to_file0.txt'
& cmd.exe /c "mklink /H `"$hlLink`" `"$hlTarget`"" | Out-Null
& cmd.exe /c "mklink /D `"$(Join-Path $root 'symlink_dir')`" `"$(Join-Path $root 'dir000')`"" | Out-Null

if ($DuFixture) {
    # M5 `ef du` size-source validation set. Precisely-known sizes so the T: subtree total
    # and the allocated-vs-real / hardlink / reparse semantics can be checked against the
    # numbers probe_du --verify prints. Cluster size is pinned to $ClusterBytes above.
    Write-Host "Planting -DuFixture size-validation files (cluster=$ClusterBytes) ..."

    # (1) Known-total subtree: du_known/. Expected @4096-byte clusters:
    #     f_4096  alloc 4096 real 4096 | f_5000 alloc 8192 real 5000
    #     sub/f_10000 alloc 12288 real 10000 | sub/f_1 resident (alloc measured, real 1)
    #     => du_known Σreal = 19097; Σalloc(non-resident) = 24576 (f_1 resident adds ~0).
    $known = Join-Path $root 'du_known'
    New-Item -ItemType Directory -Path (Join-Path $known 'sub') -Force | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $known 'f_4096.bin'), (New-Object byte[] 4096))
    [IO.File]::WriteAllBytes((Join-Path $known 'f_5000.bin'), (New-Object byte[] 5000))
    [IO.File]::WriteAllBytes((Join-Path $known 'sub\f_10000.bin'), (New-Object byte[] 10000))
    [IO.File]::WriteAllBytes((Join-Path $known 'sub\f_1.bin'), (New-Object byte[] 1))

    # (2) Sparse file: real 1 MiB, allocated ~0 (no clusters); proves allocated << real.
    $sparse = Join-Path $root 'sparse.bin'
    $fs = [IO.File]::Create($sparse); $fs.Close()
    & fsutil.exe sparse setflag "$sparse" | Out-Null
    $fs = [IO.File]::Open($sparse, 'Open', 'ReadWrite'); $fs.SetLength(1MB); $fs.Close()

    # (3) Compressed file: 1 MiB of zeros, NTFS-compressed; allocated < real.
    $comp = Join-Path $root 'compressed.bin'
    [IO.File]::WriteAllBytes($comp, (New-Object byte[] 1048576))
    & compact.exe /c "$comp" | Out-Null

    # (4) ADS host: 4096-byte main stream + a 2048-byte alternate data stream `:extra`.
    #     Tests whether the dir-enum AllocationSize includes the ADS (it should not; the ADS
    #     is a separate $DATA). WriteAllBytes to "path:stream" writes the ADS at the Win32 layer.
    $ads = Join-Path $root 'ads_host.txt'
    [IO.File]::WriteAllBytes($ads, (New-Object byte[] 4096))
    # .NET File.WriteAllBytes rejects the `path:stream` syntax; use PowerShell's -Stream.
    Set-Content -LiteralPath $ads -Stream 'extra' -Value (New-Object byte[] 2048) -Encoding Byte

    # (5) Junction (reparse point): must NOT be recursed into (target counted at its real place).
    & cmd.exe /c "mklink /J `"$(Join-Path $root 'junction_dir')`" `"$(Join-Path $root 'dir000')`"" | Out-Null

    # (6) Make the hardlink target NON-EMPTY so once-vs-twice counting is observable: the pair
    #     `hardlink_to_file0.txt` <-> `dir000\level2\level3\file_000000.txt` shares one FRN, so a
    #     correct du counts its 20480-byte allocation ONCE (a per-name walk would count it twice).
    $hlData = Join-Path $root 'dir000\level2\level3\file_000000.txt'
    if (Test-Path -LiteralPath $hlData) {
        [IO.File]::WriteAllBytes($hlData, (New-Object byte[] 20000)) # 20000 B -> 5 clusters -> alloc 20480
    }

    Write-Host 'DuFixture planted (du_known/, sparse.bin, compressed.bin, ads_host.txt, junction_dir, non-empty hardlink).'
}

Write-Host ''
Write-Host "Done. NTFS fixture mounted at ${letter}: with $made files."
Write-Host "Run probes/tests against volume '${letter}:'."
Write-Host "Tear down with:  scripts\make-test-volume.ps1 -Detach"
