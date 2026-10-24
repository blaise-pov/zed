$ErrorActionPreference = "Stop"

Write-Host "Your PATH entries:"
$env:Path -split ";" | ForEach-Object { Write-Host "  $_" }

$needAddWorkspace = $false
if ($args -notcontains "-p" -and $args -notcontains "--package")
{
    $needAddWorkspace = $true
}

# https://stackoverflow.com/questions/41324882/how-to-run-a-powershell-script-with-verbose-output/70020655#70020655
# Set-PSDebug -Trace 2

if ($env:CARGO)
{
    $Cargo = $env:CARGO
} elseif (Get-Command "cargo" -ErrorAction SilentlyContinue)
{
    $Cargo = "cargo"
} else
{
    Write-Error "Could not find cargo in path." -ErrorAction Stop
}

$clippyArgs = @($args)
if ($needAddWorkspace)
{
    $clippyArgs += "--workspace"
}
$clippyArgs += @("--release", "--all-targets", "--all-features", "--", "--deny", "warnings")

# Route through rtk (compact output proxy) when it's on PATH; an explicit $env:CARGO bypasses it.
if ($Cargo -eq "cargo" -and (Get-Command "rtk" -ErrorAction SilentlyContinue)) {
    rtk cargo clippy @clippyArgs
} else {
    & $Cargo clippy @clippyArgs
}
