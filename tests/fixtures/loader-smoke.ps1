# Builds a tiny PE executable and runs it through the real `victory run` path.
# Requires Windows, MinGW-w64 gcc, and the repository's Rust toolchain.
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$gcc = (Get-Command gcc -ErrorAction Stop).Source
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargo)) {
    throw 'cargo.exe was not found'
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) (
    'victory-loader-smoke-' + [guid]::NewGuid().ToString('N')
)
New-Item -ItemType Directory -Path $work | Out-Null
$source = Join-Path $work 'loader-smoke.c'
$exe = Join-Path $work 'loader-smoke.exe'
$cache = Join-Path $work 'cache'
$data = Join-Path $work 'data'

@'
#include <windows.h>

static const char starting[] = "loader smoke started\n";
static const char finished[] = "loader smoke completed\n";
static const char marker[] = "loader smoke ok\n";
/* Keep an address relocation so the PE can move away from its preferred base. */
static const char * volatile starting_address = starting;

DWORD WINAPI smoke_entry(void *unused) {
    (void)unused;
    DWORD written = 0;
    HANDLE output = GetStdHandle(STD_OUTPUT_HANDLE);
    WriteFile(output, starting_address, sizeof(starting) - 1, &written, NULL);

    HMODULE self = GetModuleHandleW(NULL);
    if (GetCurrentProcessId() == 0 || self == NULL ||
        self != GetModuleHandleW(L"loader-smoke.exe"))
        return 11;
    HMODULE runtime = LoadLibraryW(L"xgameruntime.dll");
    if (runtime == NULL)
        return 12;
    typedef LONG (WINAPI *initialize_fn)(void);
    initialize_fn initialize = (initialize_fn)GetProcAddress(
        runtime, "XGameRuntimeInitialize"
    );
    if (initialize == NULL || initialize() != 0)
        return 13;
    FreeLibrary(runtime);

    HANDLE file = CreateFileW(
        L"T:\\smoke-result.txt", GENERIC_WRITE, 0, NULL,
        CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL
    );
    if (file == INVALID_HANDLE_VALUE)
        return 14;
    BOOL wrote = WriteFile(file, marker, sizeof(marker) - 1, &written, NULL);
    CloseHandle(file);
    if (!wrote || written != sizeof(marker) - 1)
        return 15;
    if (GetFileAttributesW(L"T:\\smoke-result.txt") == INVALID_FILE_ATTRIBUTES)
        return 16;

    WriteFile(output, finished, sizeof(finished) - 1, &written, NULL);
    return 0;
}
'@ | Set-Content -LiteralPath $source -Encoding ascii

& $gcc -nostdlib '-Wl,-e,smoke_entry' '-Wl,--subsystem,console' `
    '-Wl,--dynamicbase' '-Wl,--image-base,0x180000000' `
    -o $exe $source -lkernel32
if ($LASTEXITCODE -ne 0) { throw 'building the sample executable failed' }

& $cargo build --quiet --manifest-path (Join-Path $repo 'Cargo.toml') `
    --target-dir (Join-Path $repo 'target') -p victory
if ($LASTEXITCODE -ne 0) { throw 'building victory failed' }
$victory = Join-Path $repo 'target\debug\victory.exe'

$env:VICTORY_CACHE_DIR = $cache
$env:VICTORY_DATA_DIR = $data
Remove-Item Env:VICTORY_CONFIG -ErrorAction SilentlyContinue
Push-Location $work
try {
    Write-Host '--- prepare, map, bind ---'
    & $victory run $exe
    if ($LASTEXITCODE -ne 0) { throw 'binding the sample failed' }
    Write-Host '--- execute ---'
    & $victory run $exe --execute
    if ($LASTEXITCODE -ne 0) { throw 'executing the sample failed' }
} finally {
    Pop-Location
}

$result = Join-Path $data '00000000\temp\smoke-result.txt'
if ((Get-Content -LiteralPath $result -Raw).Trim() -ne 'loader smoke ok') {
    throw "sample result was missing or incorrect: $result"
}
Write-Host "Sample passed; output: $result"
