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

# Route through rtk (compact output proxy) when it's on PATH; an explicit $env:CARGO bypasses it.
function Invoke-Clippy
{
    if ($script:Cargo -eq "cargo" -and (Get-Command "rtk" -ErrorAction SilentlyContinue))
    {
        rtk cargo clippy @args
    }
    else
    {
        & $script:Cargo clippy @args
    }
}

if ($needAddWorkspace)
{
    Invoke-Clippy @args --workspace --release --all-targets --all-features -- --deny warnings
} else
{
    Invoke-Clippy @args --release --all-targets --all-features -- --deny warnings
}
