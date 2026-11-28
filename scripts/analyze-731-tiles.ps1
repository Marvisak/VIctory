# Inspect the packed tile-list SRV captured beside a 731 world frame.
# The shader selects partition 3 of four, with 3,645 tiles at 1280x720.
param(
    [Parameter(Mandatory = $true)] [string] $Capture,
    [ValidateRange(1, 7680)] [int] $Width = 1280,
    [ValidateRange(1, 4320)] [int] $Height = 720,
    [ValidateRange(0, 15)] [int] $Partition = 3,
    [ValidateRange(1, 1000000)] [int] $PartitionSize = 3645
)

$ErrorActionPreference = 'Stop'
$path = (Resolve-Path -LiteralPath $Capture).Path
$bytes = [IO.File]::ReadAllBytes($path)
if ($bytes.Length % 4 -ne 0) { throw "Capture has $($bytes.Length) bytes, not whole tile words" }
$wordCount = [int]($bytes.Length / 4)
$first = $Partition * $PartitionSize
$last = $first + $PartitionSize
if ($last -gt $wordCount) { throw "Partition $Partition needs $last words; capture has $wordCount" }

$counts = @{}
$zeroEntries = 0
$outside = 0
$unaligned = 0
$outsideExamples = [Collections.Generic.List[object]]::new()
for ($i = $first; $i -lt $last; $i++) {
    $word = [BitConverter]::ToUInt32($bytes, $i * 4)
    if ($word -eq 0) {
        $zeroEntries++
        continue
    }
    $x = [int](($word -band 0xffff) * 2)
    $y = [int](($word -shr 16) * 2)
    $key = "$x,$y"
    if ($counts.ContainsKey($key)) { $counts[$key]++ } else { $counts[$key] = 1 }
    if (($x % 16) -ne 0 -or ($y % 16) -ne 0) { $unaligned++ }
    if (($x + 15) -ge $Width -or ($y + 15) -ge $Height) {
        $outside++
        if ($outsideExamples.Count -lt 16) { $outsideExamples.Add(@($x, $y)) }
    }
}

$missing = 0
$missingExamples = [Collections.Generic.List[object]]::new()
for ($y = 0; $y -lt $Height; $y += 16) {
    for ($x = 0; $x -lt $Width; $x += 16) {
        if (-not $counts.ContainsKey("$x,$y") -and -not ($x -eq 0 -and $y -eq 0 -and $zeroEntries -gt 0)) {
            $missing++
            if ($missingExamples.Count -lt 16) { $missingExamples.Add(@($x, $y)) }
        }
    }
}
$repeated = @($counts.GetEnumerator() | Where-Object { $_.Value -gt 1 } |
    Sort-Object Value -Descending | Select-Object -First 8 |
    ForEach-Object { [pscustomobject]@{ coordinate = $_.Key; count = $_.Value } })
[pscustomobject]@{
    path = $path
    captureWords = $wordCount
    partition = $Partition
    partitionSize = $PartitionSize
    target = @($Width, $Height)
    zeroEntries = $zeroEntries
    uniqueNonzeroTiles = $counts.Count
    duplicateNonzeroEntries = $PartitionSize - $zeroEntries - $counts.Count
    unalignedNonzeroEntries = $unaligned
    outsideNonzeroEntries = $outside
    outsideExamples = @($outsideExamples.ToArray())
    unlistedTargetTiles = $missing
    unlistedExamples = @($missingExamples.ToArray())
    mostRepeatedNonzero = $repeated
} | ConvertTo-Json -Depth 5
