# Embedding batch-size benchmark, Windows. See embed-batch-bench.sh for the
# method; this is the same run with the peak working set sampled from the
# process every 200 ms.
#
# Usage: pwsh bench/embed-batch-bench.ps1 [-Sizes 8,16,32]   (default: 8 16 32 64 128 256)
param([int[]]$Sizes = @(8, 16, 32, 64, 128, 256))
$ErrorActionPreference = 'Stop'
$exe = Join-Path (Split-Path $PSScriptRoot -Parent) 'target\release\rearview.exe'
$real = Join-Path $env:USERPROFILE '.cache\rearview\semantic'
$bench = Join-Path $env:USERPROFILE '.cache\rearview-embed-bench'
$pending = Join-Path $bench 'pending'    # the real cache as of now: chunks still to embed
$complete = Join-Path $bench 'complete'  # after one full catch-up: nothing to embed
$run = Join-Path $bench 'run'
$logs = Join-Path $bench 'logs'

if (-not (Test-Path $pending)) {
    New-Item -ItemType Directory -Force $bench | Out-Null
    Copy-Item -Recurse $real $pending
}
New-Item -ItemType Directory -Force $logs | Out-Null

function Invoke-Run([string]$label, [string]$source, [int]$batch) {
    if (Test-Path $run) { Remove-Item -Recurse -Force $run }
    Copy-Item -Recurse $source $run
    $env:REARVIEW_BENCH_SEMANTIC_DIR = $run
    $env:REARVIEW_BENCH_BATCH = $batch
    $err = Join-Path $logs "$label.stderr.txt"
    $out = Join-Path $logs "$label.stdout.txt"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $exe -ArgumentList '--generate-semantic-cache' `
        -RedirectStandardError $err -RedirectStandardOutput $out -PassThru -NoNewWindow
    $peak = 0L
    while (-not $p.HasExited) {
        try { $p.Refresh(); if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 } } catch {}
        Start-Sleep -Milliseconds 200
    }
    $sw.Stop()
    $text = Get-Content $err -Raw
    $embedded = [regex]::Matches($text, 'embedded (\d+)/(\d+)') | Select-Object -Last 1
    [pscustomobject]@{
        run      = $label
        batch    = $batch
        embedded = if ($embedded) { $embedded.Groups[2].Value } else { '0' }
        seconds  = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        peak_MB  = [math]::Round($peak / 1MB)
        exit     = $p.ExitCode
    }
}

$results = @()
# The first full catch-up at the default batch; its output is the zero-miss control.
$results += Invoke-Run 'b32-first' $pending 32
if (Test-Path $complete) { Remove-Item -Recurse -Force $complete }
Copy-Item -Recurse $run $complete
$results += Invoke-Run 'control-no-misses' $complete 32
foreach ($b in $Sizes) {
    $results += Invoke-Run "b$b" $pending $b
}
$results | Format-Table -AutoSize | Out-String -Width 120
$results | Export-Csv -NoTypeInformation (Join-Path $logs 'results.csv')
Remove-Item -Recurse -Force $run
