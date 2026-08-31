<#
.SYNOPSIS
    Drive the README demo at a filmable pace, so recording it is one keypress and one run.

.DESCRIPTION
    A GIF in a README is silent, loops, and is read in a few seconds, so the demo has to make
    its point without narration and without a single fumbled keystroke. This types each command
    out character by character (a live-typing look, but never a typo), runs it, and holds the
    result long enough to read.

    Hit record, run this, stop recording. Nothing here is interactive, so a retake costs a
    rerun rather than a rehearsal.

    **No stopwatch on screen.** Timing `ef` through a shell measures the shell too: PowerShell
    adds about 55 ms of its own, so `Measure-Command { ef … }` reports ~79 ms where the README
    reports ~15 ms end-to-end from a `CreateProcess`-direct harness. Both numbers are honest
    and the README explains the difference, but a GIF cannot carry a footnote, and a figure
    that contradicts the README costs more than it buys. Results appearing the instant Enter
    is pressed is the demo.

    The TUI is deliberately NOT scripted: driving a full-screen app with synthetic keystrokes
    looks like what it is. Record that part separately, by hand, if you want it.

.PARAMETER Speed
    Multiplier on every pause. 1 is the tuned pace; 0.5 is twice as fast for a shorter GIF.

.PARAMETER Query
    The one-shot search to show. `readme` is the default because every hit lands in
    `C:\Program Files\Go\...`: real output with none of your own directory names in it. A query
    like `cargo.toml` reads better (fewer hits, shorter paths) at the cost of showing what you
    keep in `C:\Users`; that is a choice to make on purpose, not by default.

.EXAMPLE
    .\demo.ps1
.EXAMPLE
    .\demo.ps1 -Speed 0.7 -Query cargo.toml
#>
[CmdletBinding()]
param(
    [double] $Speed = 1.0,
    [string] $Query = 'readme'
)

$ErrorActionPreference = 'Stop'

function Pause-For($seconds) { Start-Sleep -Milliseconds ([int]($seconds * 1000 * $Speed)) }

# A prompt that reads as a prompt in a GIF without showing anyone's directory tree.
function Prompt-Line { Write-Host "PS> " -NoNewline -ForegroundColor DarkCyan }

# Type it out, so the viewer sees a command being entered rather than appearing.
function Type-Command($text, $cps = 22) {
    Prompt-Line
    foreach ($ch in $text.ToCharArray()) {
        Write-Host $ch -NoNewline
        Start-Sleep -Milliseconds ([int]((1000 / $cps) * $Speed))
    }
    Write-Host ""
}

# `Out-Host` rather than letting the output fall into the pipeline: `Write-Host` goes straight
# to the console, so if the command's own output is buffered anywhere the two interleave
# wrongly and the recording shows the prompts before the results.
function Show($command, $hold = 2.2) {
    Type-Command $command
    Invoke-Expression $command | Out-Host
    Pause-For $hold
    Write-Host ""
}

Clear-Host
Pause-For 0.6

# 1. Establish the scale first. "Instant" means nothing until the viewer knows what it is
#    instant over: six and a half million files, live.
Show 'ef status' 3.2

# 2. The search itself. No timer: the results are simply there when Enter lands.
Show "ef $Query -n 10" 3.2

# 3. Disk usage from the same index, which is the part people do not expect.
Show 'ef du -n 8' 3.5

Write-Host ""
Write-Host "  (stop recording here)" -ForegroundColor DarkGray
Pause-For 1.0
