param(
    [string]$Elf = "c:\Users\ZhangZehu\Documents\ADS\Testing\tcf-agent\temp\TC375_CE\TriCore Debug (GCC)\TC375_CE.elf",
    [string]$Device = "DAS JDS AURIX LITE KIT V2.0 (TC375) LK8PIMU6",
    [string]$Workspace = "c:\Users\ZhangZehu\Documents\ADS\Testing\tcf-agent\temp\TC375_CE",
    [string]$Core0Source = "c:\Users\ZhangZehu\Documents\ADS\Testing\tcf-agent\temp\TC375_CE\TC375_CE\src\Cpu0_Main.c",
    [int]$Core0Line= 37,
    [string]$StopAtFunction = "test_main_loop",
    [string]$Core0Function = "core0_main"
)

# Populate from env if params are not provided.
if (-not $Elf) { $Elf = $env:AURIX_TEST_ELF }
if (-not $Device) { $Device = $env:AURIX_TEST_DEVICE }
if (-not $Workspace) { $Workspace = $env:AURIX_TEST_WORKSPACE }
if (-not $Core0Source) { $Core0Source = $env:AURIX_TEST_CORE0_SOURCE }
if (-not $Core0Line) { $Core0Line = [int]$env:AURIX_TEST_CORE0_LINE }
if (-not $StopAtFunction) { $StopAtFunction = $env:AURIX_TEST_STOP_AT_FUNCTION }
if (-not $Core0Function) { $Core0Function = $env:AURIX_TEST_CORE0_FUNCTION }

if (-not $Elf) { throw "Missing ELF path. Set -Elf or AURIX_TEST_ELF." }
if (-not $Core0Source) { throw "Missing core0 source path. Set -Core0Source or AURIX_TEST_CORE0_SOURCE." }
if (-not $Core0Line -or $Core0Line -le 0) { throw "Missing/invalid core0 line. Set -Core0Line or AURIX_TEST_CORE0_LINE (1-based)." }

# Required vars for breakpoint.stopat.test.ts
$env:AURIX_TEST_ELF = $Elf
if ($Device) { $env:AURIX_TEST_DEVICE = $Device }
if ($Workspace) { $env:AURIX_TEST_WORKSPACE = $Workspace }
$env:AURIX_TEST_STOP_AT_FUNCTION = $StopAtFunction
$env:AURIX_TEST_CORE0_SOURCE = $Core0Source
$env:AURIX_TEST_CORE0_LINE = "$Core0Line"
$env:AURIX_TEST_CORE0_FUNCTION = $Core0Function
$env:AURIX_TEST_REPORT = '1'
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
$env:MOCHA_FILE = Join-Path $repoRoot 'test-results\junit.xml'

# Keep the stress test from doing any iterations.
$env:AURIX_TEST_RUNS = "0"

# Avoid VS Code integrated terminal env leaking into test host.
$env:ELECTRON_RUN_AS_NODE = ''

# Suppress Node.js warnings in the extension host.
if ($env:NODE_OPTIONS) {
    $env:NODE_OPTIONS = ''
}
$env:NODE_NO_WARNINGS = '1'

npm run test
exit $LASTEXITCODE
