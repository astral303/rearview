# Serves scripts/compare-perf.sh on Windows: runs each native binary and reads
# the disks' byte counters. The binary runs here because bash's `time` does
# not count a native Windows program's CPU time; this process reads it from
# the binary's handle after it exits.
#
# Requests, each answered with one line:
#   disk      -> `-` when the disk counters cannot be read; otherwise the bytes
#                every physical disk has read and written since boot, then the
#                disks' names
#   run N     -> followed by N lines: the cache directory, the binary and its
#                arguments. Replies with the binary's exit code, wall time and
#                CPU time, then this helper's own CPU time while serving the
#                request, all times in ms.
#   stderr F  -> writes the last run's stderr to file F, then replies `ok`.
#                Kept out of `run`, so that the write happens after the
#                script's CPU samples.
#
# Closing the input or pressing Ctrl+C ends a running binary and the helper.
$ErrorActionPreference = 'Stop'
$childExitWaitMs = 5000
$utf8 = New-Object System.Text.UTF8Encoding $false
$helperProcess = [System.Diagnostics.Process]::GetCurrentProcess()
# Test-only: the counter category to read, to make the counters unavailable.
$diskCategory = if ($env:COMPARE_PERF_DISK_COUNTER_CATEGORY) { $env:COMPARE_PERF_DISK_COUNTER_CATEGORY } else { 'PhysicalDisk' }

# PhysicalDisk counts a mounted virtual disk as well as the disk holding it.
function Open-DiskCounters {
    try {
        $counters = @{
            Read = New-Object System.Diagnostics.PerformanceCounter $diskCategory, 'Disk Read Bytes/sec', '_Total', $true
            Written = New-Object System.Diagnostics.PerformanceCounter $diskCategory, 'Disk Write Bytes/sec', '_Total', $true
            Disks = New-Object System.Diagnostics.PerformanceCounterCategory $diskCategory
        }
        [void] ($counters.Read.RawValue + $counters.Written.RawValue)
        $counters
    } catch [System.InvalidOperationException], [System.UnauthorizedAccessException], [System.ComponentModel.Win32Exception] {
        $null
    }
}

# The sample is `-` when a counter read fails, as after a disk's removal.
function Get-DiskSample($counters) {
    if ($null -eq $counters) { return '-' }
    try {
        $names = (@($counters.Disks.GetInstanceNames() | Sort-Object) -join ',') -replace ' ', '_'
        '{0} {1}' -f ($counters.Read.RawValue + $counters.Written.RawValue), $names
    } catch [System.InvalidOperationException], [System.UnauthorizedAccessException], [System.ComponentModel.Win32Exception] {
        '-'
    }
}

# Waits in short steps so that Ctrl+C can stop the helper meanwhile.
function Receive-TaskResult($task) {
    while (-not $task.Wait(100)) { }
    $task.Result
}

# Quotes one argument for the Rust runtime's command-line parser.
function ConvertTo-CommandLineArgument([string] $argument) {
    '"' + ($argument -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

# Returns the run's reply fields and stderr, or $null when $nextRequest shows
# the input closed before the binary exited; the binary is then ended.
function Invoke-Binary([string] $cacheDirectory, [string] $binary, [string[]] $arguments, $nextRequest) {
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $binary
    $startInfo.Arguments = (@($arguments) | ForEach-Object { ConvertTo-CommandLineArgument $_ }) -join ' '
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardErrorEncoding = $utf8
    $startInfo.EnvironmentVariables['REARVIEW_CACHE_DIR'] = $cacheDirectory
    $process = $null
    try {
        $clock = [System.Diagnostics.Stopwatch]::StartNew()
        $process = [System.Diagnostics.Process]::Start($startInfo)
        $process.StandardInput.Close()
        $stdout = $process.StandardOutput.BaseStream.CopyToAsync([System.IO.Stream]::Null)
        $stderr = $process.StandardError.ReadToEndAsync()
        while (-not $process.WaitForExit(100)) {
            if ($nextRequest.IsCompleted -and $null -eq $nextRequest.Result) { return $null }
        }
        $clock.Stop()
        $stdout.Wait()
        @{
            Figures = '{0} {1} {2}' -f $process.ExitCode, [int64] $clock.Elapsed.TotalMilliseconds,
                [int64] $process.TotalProcessorTime.TotalMilliseconds
            Stderr = $stderr.Result
        }
    } finally {
        if ($null -ne $process) {
            if (-not $process.HasExited) {
                try { $process.Kill() } catch [System.InvalidOperationException] { }
                [void] $process.WaitForExit($childExitWaitMs)
            }
            $process.Dispose()
        }
    }
}

$requests = New-Object System.IO.StreamReader ([Console]::OpenStandardInput()), $utf8
$replies = New-Object System.IO.StreamWriter ([Console]::OpenStandardOutput()), $utf8
$replies.NewLine = "`n"
$replies.AutoFlush = $true
$diskCounters = Open-DiskCounters
$lastStderr = ''
try {
    # One read is always pending: during a run it reports the input closing.
    $nextRequest = $requests.ReadLineAsync()
    while ($true) {
        $cpuBefore = $helperProcess.TotalProcessorTime
        $request = Receive-TaskResult $nextRequest
        if ($null -eq $request) { break }
        if ($request -eq 'disk') {
            $nextRequest = $requests.ReadLineAsync()
            $replies.WriteLine((Get-DiskSample $diskCounters))
            continue
        }
        if ($request -match '^stderr (.+)$') {
            $nextRequest = $requests.ReadLineAsync()
            [System.IO.File]::WriteAllText($Matches[1], $lastStderr, $utf8)
            $replies.WriteLine('ok')
            continue
        }
        if ($request -notmatch '^run (\d+)$') { throw "unknown request: $request" }
        $fields = @(1..[int] $Matches[1] | ForEach-Object { Receive-TaskResult $requests.ReadLineAsync() })
        $nextRequest = $requests.ReadLineAsync()
        $run = Invoke-Binary $fields[0] $fields[1] @($fields | Select-Object -Skip 2) $nextRequest
        if ($null -eq $run) { break }
        $lastStderr = $run.Stderr
        $helperCpuMs = [int64] ($helperProcess.TotalProcessorTime - $cpuBefore).TotalMilliseconds
        $replies.WriteLine(('{0} {1}' -f $run.Figures, $helperCpuMs))
    }
} finally {
    if ($null -ne $diskCounters) {
        $diskCounters.Read.Dispose()
        $diskCounters.Written.Dispose()
    }
    $replies.Dispose()
    $requests.Dispose()
}
