param(
    [Parameter(Mandatory = $true)][string]$BinaryRoot,
    [Parameter(Mandatory = $true)][string]$OutputCsv,
    [int]$Pairs = 5,
    [int]$Samples = 9,
    [long]$Affinity = 16,
    [string[]]$Suites = @('in_memory', 'algorithm_paths', 'leaf_tests'),
    [string[]]$Revisions = @('baseline', 'candidate'),
    [switch]$Phases
)

# Build both revisions first. Each directory must contain frozen executables;
# no compilation or other tests should run during the comparison.
$ErrorActionPreference = 'Stop'
if ($Pairs -lt 1 -or $Samples -lt 1) { throw 'Pairs and Samples must be positive' }
if ($Revisions.Count -lt 2 -or @($Revisions | Select-Object -Unique).Count -ne $Revisions.Count) {
    throw 'Provide at least two distinct revision directories'
}
[System.Diagnostics.Process]::GetCurrentProcess().ProcessorAffinity = [IntPtr]$Affinity
$env:BFTREE_BENCH_SAMPLES = "$Samples"
$env:BFTREE_BENCH_FILTER = ''
if ($Phases) { $env:BFTREE_BENCH_PHASES = '1' }
else { Remove-Item Env:BFTREE_BENCH_PHASES -ErrorAction SilentlyContinue }
$rows = [System.Collections.Generic.List[object]]::new()
for ($pair = 1; $pair -le $Pairs; $pair++) {
    if ($Revisions.Count -eq 2) {
        $order = @($Revisions)
    } else {
        $offset = ($pair - 1) % $Revisions.Count
        $order = @(for ($i = 0; $i -lt $Revisions.Count; $i++) {
            $Revisions[($i + $offset) % $Revisions.Count]
        })
    }
    if ($pair % 2 -eq 0) { [array]::Reverse($order) }
    foreach ($revision in $order) {
        foreach ($suite in $Suites) {
            $binary = if ($suite -eq 'growth') { 'leaf_tests' } else { $suite }
            $exe = Join-Path $BinaryRoot "$revision\$binary.exe"
            if ($suite -in @('leaf_tests', 'growth')) {
                $test = if ($suite -eq 'growth') { 'leaf_growth_microbench' } else { 'leaf_merge_microbench' }
                $raw = & $exe $test --ignored --nocapture --test-threads=1
                if ($LASTEXITCODE -ne 0) { throw "$revision/$suite failed" }
                $csv = @('workload,median_ns_per_op,min_ns_per_op,max_ns_per_op')
                $pattern = if ($suite -eq 'growth') { '^growth/' } else { '^merge_' }
                $expected = if ($suite -eq 'growth') { 9 } else { 13 }
                $csv += $raw | Where-Object { $_ -match $pattern }
                if ($csv.Count -ne $expected) { throw "Missing $suite benchmark rows" }
            } else {
                $csv = & $exe
                if ($LASTEXITCODE -ne 0) { throw "$revision/$suite failed" }
            }
            foreach ($row in ($csv | ConvertFrom-Csv)) {
                $rows.Add([pscustomobject]@{
                    suite = $suite
                    workload = $row.workload
                    pair = $pair
                    revision = $revision
                    position = [array]::IndexOf($order, $revision) + 1
                    samples = $Samples
                    affinity_mask = $Affinity
                    median_ns = $row.median_ns_per_op
                    min_ns = $row.min_ns_per_op
                    max_ns = $row.max_ns_per_op
                })
            }
        }
        $rows | Export-Csv -NoTypeInformation -LiteralPath $OutputCsv
        Write-Output "pair $pair/$Pairs $revision complete"
    }
}
