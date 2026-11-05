# Runs the whole gate in one go: format, lint, tests, wasm build.
#
# Four separate invocations that have to all pass, in that order, because each one
# catches things the previous one does not: `fmt` because a misformatted diff is noise
# in review, `clippy` because the default warnings are quiet and `-D warnings` is not,
# `test` because that is the only place behaviour is checked, and the wasm build because
# the plugin target compiles differently from the host and a host-only pass proves
# nothing about the artifact that actually ships.
#
# Usage:  .\check.ps1
#         .\check.ps1 -Fast      # skip the wasm build, which is the slow part

[CmdletBinding()]
param(
    [switch]$Fast
)

$ErrorActionPreference = "Continue"
# Scoped to the script, not a plain local: `$failed += $name` inside a function would
# otherwise create the function's own copy and the caller would never see it — which is
# how three failing steps came to report "all green".
$script:failed = @()

function Step($name, [scriptblock]$body) {
    Write-Host "`n=== $name ===" -ForegroundColor Cyan
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    # `2>&1` folds the error stream into the captured output. Without it a passing cargo
    # run still dumps its progress lines to the error stream, where PowerShell renders
    # them as red NativeCommandError blocks directly under a green "ok" — which reads as
    # a failure of exactly the step that passed.
    $output = & $body 2>&1
    # Written straight to the host so it streams as it goes, rather than being
    # captured and re-emitted after the fact.
    if ($output) { $output | Write-Host }
    # Captured *immediately*, before anything else runs: `$LASTEXITCODE` is a
    # global that any command in between will overwrite, which is how a failed step
    # came to report success.
    $code = $LASTEXITCODE
    $sw.Stop()
    if ($code -ne 0) {
        $script:failed += $name
        Write-Host "FAILED ($([int]$sw.Elapsed.TotalSeconds)s)" -ForegroundColor Red
    }
    else {
        Write-Host "ok ($([int]$sw.Elapsed.TotalSeconds)s)" -ForegroundColor Green
    }
    # Restored so the next step starts from a clean slate.
    $global:LASTEXITCODE = 0
}

$start = Get-Date

Step "format" { cargo fmt --all -- --check }
Step "clippy" { cargo clippy --workspace --all-targets }
Step "tests"  { cargo test --workspace }

if ($Fast) {
    Write-Host "`n(skipping the wasm build; -Fast was given)" -ForegroundColor DarkGray
}
else {
    Step "wasm" {
        cargo build --release --target wasm32-wasip2 -p worldedit-pumpkin 2>&1 |
            Select-String -Pattern "error|warning"
    }
    $wasm = Get-Item "target\wasm32-wasip2\release\worldedit_pumpkin.wasm" -ErrorAction SilentlyContinue
    if ($wasm) {
        Write-Host "artifact: $wasm ($([int]($wasm.Length / 1kb)) KB)" -ForegroundColor DarkGray
    }
}

Write-Host "`n=== summary ===" -ForegroundColor Cyan
$total = [int]((Get-Date) - $start).TotalSeconds
if ($script:failed.Count -eq 0) {
    Write-Host "all green in ${total}s" -ForegroundColor Green
    exit 0
}
Write-Host "failed: $($script:failed -join ', ')" -ForegroundColor Red
exit 1
