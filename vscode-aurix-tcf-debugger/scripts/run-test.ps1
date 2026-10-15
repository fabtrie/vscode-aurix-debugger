param(
    [string]$Elf = "c:\\Users\\ZhangZehu\\Documents\\ADS\\Testing\\tcf-agent\\temp\\TC375_CE\\TriCore Debug (GCC)\\TC375_CE.elf",
    [string]$Device = "DAS JDS AURIX LITE KIT V2.0 (TC375) LK8PIMU6",
    [string]$ExpectedSource,
    [string]$ExpectedEntry = "core0_main",
    [int]$Runs = 10,
    [int]$RequireCpu0 = 0,
    [string]$Workspace = "c:\\Users\\ZhangZehu\\Documents\\ADS\\Testing\\tcf-agent\\temp\\TC375_CE"
)

# Populate env from params (or keep existing env values if params are empty).
if (-not $Elf) { $Elf = $env:AURIX_TEST_ELF }
if (-not $Device) { $Device = $env:AURIX_TEST_DEVICE }
if (-not $ExpectedSource) { $ExpectedSource = $env:AURIX_TEST_EXPECTED_SOURCE }
if (-not $ExpectedEntry) { $ExpectedEntry = $env:AURIX_TEST_ENTRY }
if (-not $Workspace) { $Workspace = $env:AURIX_TEST_WORKSPACE }

if ($Elf) { $env:AURIX_TEST_ELF = $Elf }
if ($Device) { $env:AURIX_TEST_DEVICE = $Device }
if ($ExpectedSource) { $env:AURIX_TEST_EXPECTED_SOURCE = $ExpectedSource }
if ($ExpectedEntry) { $env:AURIX_TEST_ENTRY = $ExpectedEntry }
if ($Workspace) { $env:AURIX_TEST_WORKSPACE = $Workspace }

if ($Runs -gt 0) { $env:AURIX_TEST_RUNS = "$Runs" }
if ($RequireCpu0 -ne 0) { $env:AURIX_TEST_REQUIRE_CPU0 = "$RequireCpu0" }

# Avoid VS Code integrated terminal env leaking into test host.
$env:ELECTRON_RUN_AS_NODE = ''

# Suppress Node.js warnings (e.g. punycode deprecation) in the extension host.
# NOTE: VS Code is a packaged Electron app and prints an error when NODE_OPTIONS is set.
if ($env:NODE_OPTIONS) {
    $env:NODE_OPTIONS = ''
}
$env:NODE_NO_WARNINGS = '1'

npm run test
exit $LASTEXITCODE
