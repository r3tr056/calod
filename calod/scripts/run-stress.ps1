param(
    [ValidateSet("SMOKE", "RELEASE", "A", "B", "C")]
    [string]$Profile = "A",
    [ValidateSet("Calod", "Redis", "Both")]
    [string]$Target = "Calod",
    [string]$ServerMemLimit = "6g",
    [string]$RunnerMemLimit = "1g",
    [string]$ServerCpus = "4",
    [string]$RunnerCpus = "2",
    [int]$DurationSecs = 0,
    [int]$Concurrency = 0,
    [int]$KeyCount = 0,
    [int]$RepeatRuns = 1,
    [switch]$KeepRunning
)

$ErrorActionPreference = "Stop"

function Invoke-Step([string]$Command) {
    Invoke-Expression $Command
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

function Convert-ToMiB([string]$Value) {
    if ($Value -match '^(?<num>\d+)(?<unit>[kKmMgG])$') {
        $num = [int]$Matches['num']
        $unit = $Matches['unit'].ToLowerInvariant()
        switch ($unit) {
            'k' { return [math]::Ceiling($num / 1024.0) }
            'm' { return $num }
            'g' { return $num * 1024 }
        }
    }
    throw "Unsupported memory limit format '$Value'. Use values like 512m, 1g, 6g."
}

$serverMiB = Convert-ToMiB $ServerMemLimit
$runnerMiB = Convert-ToMiB $RunnerMemLimit
if (($serverMiB + $runnerMiB) -gt 8192) {
    throw "Combined Docker memory budget exceeds 8GB: server=$ServerMemLimit runner=$RunnerMemLimit"
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$artifactsDir = Join-Path $repoRoot "stress-artifacts"
New-Item -ItemType Directory -Force $artifactsDir | Out-Null

$env:STRESS_PROFILE = $Profile
$env:CALOD_SERVER_MEM_LIMIT = $ServerMemLimit
$env:CALOD_RUNNER_MEM_LIMIT = $RunnerMemLimit
$env:CALOD_SERVER_CPUS = $ServerCpus
$env:CALOD_RUNNER_CPUS = $RunnerCpus
if ($DurationSecs -gt 0) { $env:STRESS_DURATION_SECS = "$DurationSecs" } else { Remove-Item Env:STRESS_DURATION_SECS -ErrorAction SilentlyContinue }
if ($Concurrency -gt 0) { $env:STRESS_CONCURRENCY = "$Concurrency" } else { Remove-Item Env:STRESS_CONCURRENCY -ErrorAction SilentlyContinue }
if ($KeyCount -gt 0) { $env:STRESS_KEY_COUNT = "$KeyCount" } else { Remove-Item Env:STRESS_KEY_COUNT -ErrorAction SilentlyContinue }
if ($RepeatRuns -gt 1) { $env:STRESS_REPEAT_RUNS = "$RepeatRuns" } else { Remove-Item Env:STRESS_REPEAT_RUNS -ErrorAction SilentlyContinue }

Push-Location $repoRoot
try {
    Invoke-Step 'docker compose -f docker-compose.stress.yml --profile redis build'

    function Invoke-BenchmarkTarget([string]$Name) {
        if ($Name -eq "Calod") {
            $service = "calod"
            $runner = "stress-runner"
            $composePrefix = "docker compose -f docker-compose.stress.yml"
            $artifactLabel = "calod"
        } else {
            $service = "redis-baseline"
            $runner = "stress-runner-redis"
            $composePrefix = "docker compose -f docker-compose.stress.yml --profile redis"
            $artifactLabel = "redis"
        }

        Invoke-Step "$composePrefix up -d $service"
        Invoke-Step "$composePrefix run --rm $runner"

        $containerId = Invoke-Expression "$composePrefix ps -q $service"
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to query $service container id"
        }
        if ($containerId) {
            Invoke-Step "docker stats --no-stream --format ""{{json .}}"" $containerId > ""$(Join-Path $artifactsDir "$Profile-$artifactLabel-docker-stats.json")"""
            Invoke-Step "docker inspect --format ""{{json .HostConfig.Memory}}"" $containerId > ""$(Join-Path $artifactsDir "$Profile-$artifactLabel-memory-limit-bytes.json")"""
        }
    }

    if ($Target -eq "Both") {
        Invoke-BenchmarkTarget "Calod"
        Invoke-BenchmarkTarget "Redis"
    } else {
        Invoke-BenchmarkTarget $Target
    }

    Write-Host "Stress test artifacts written to $artifactsDir"
}
finally {
    if (-not $KeepRunning) {
        docker compose -f docker-compose.stress.yml --profile redis down --remove-orphans | Out-Null
    }
    Pop-Location
}
