<#
Sync this fork with openai/codex, then replay and publish the custom branch.
From the repository root: pwsh -File scripts/sync-custom.ps1
From elsewhere, pass the script's absolute path to pwsh -File.

Expected remotes: origin = openai/codex, codexmyfork = your fork.
The script starts and ends on custom. If a rebase stops, resolve conflicts,
continue the rebase, then push custom with --force-with-lease.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Invoke-Git {
    param([string[]]$GitArgs)

    & git @GitArgs
    if ($LASTEXITCODE -ne 0) {
        throw "git $($GitArgs -join ' ') failed (exit $LASTEXITCODE)."
    }
}

function Test-Ancestor {
    param([string]$Older, [string]$Newer)

    & git merge-base --is-ancestor $Older $Newer
    if ($LASTEXITCODE -gt 1) {
        throw "Cannot compare $Older with $Newer (exit $LASTEXITCODE)."
    }
    return $LASTEXITCODE -eq 0
}

$repoRoot = & git -C $PSScriptRoot rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0) {
        throw 'Place this script inside the Codex repository.'
}

Push-Location $repoRoot
try {
    $branch = & git branch --show-current
    if ($LASTEXITCODE -ne 0 -or $branch -ne 'custom') {
        throw 'Start on the local custom branch.'
    }

    $changes = & git status --porcelain --untracked-files=no
    if ($LASTEXITCODE -ne 0 -or $changes) {
        throw 'Commit or stash tracked changes before syncing.'
    }

    Write-Host '1/4 Fetching official main and fork branches...'
    Invoke-Git -GitArgs @('fetch', '--no-tags', 'origin', 'main')
    Invoke-Git -GitArgs @('fetch', '--no-tags', 'codexmyfork', 'main', 'custom')

    if (-not (Test-Ancestor 'main' 'origin/main')) {
        throw 'Local main has commits outside origin/main; inspect it before syncing.'
    }
    if (-not (Test-Ancestor 'codexmyfork/main' 'origin/main')) {
        throw 'Fork main has commits outside origin/main; inspect it before syncing.'
    }
    if (-not (Test-Ancestor 'codexmyfork/custom' 'custom') -and
        -not (Test-Ancestor 'custom' 'codexmyfork/custom')) {
        throw 'Local and fork custom branches diverged; reconcile them before syncing.'
    }

    $oldCustom = & git rev-parse refs/remotes/codexmyfork/custom
    if ($LASTEXITCODE -ne 0) {
        throw 'Cannot read the fork custom branch.'
    }

    Write-Host '2/4 Fast-forwarding local main and publishing fork main...'
    Invoke-Git -GitArgs @('switch', 'main')
    Invoke-Git -GitArgs @('merge', '--ff-only', 'origin/main')
    Invoke-Git -GitArgs @('push', 'codexmyfork', 'main:refs/heads/main')

    Write-Host '3/4 Updating and rebasing custom onto main...'
    Invoke-Git -GitArgs @('switch', 'custom')
    Invoke-Git -GitArgs @('merge', '--ff-only', 'codexmyfork/custom')
    try {
        Invoke-Git -GitArgs @('rebase', 'main')
    } catch {
        throw 'Rebase stopped. Resolve conflicts, run git add <files> and git rebase --continue until it finishes, then run git push --force-with-lease codexmyfork custom. Use git rebase --abort to cancel.'
    }

    Write-Host '4/4 Publishing custom with a lease on the fetched remote commit...'
    Invoke-Git -GitArgs @('push', "--force-with-lease=refs/heads/custom:${oldCustom}", 'codexmyfork', 'custom:refs/heads/custom')
    Write-Host 'Done: local main, fork main, and custom are up to date.'
} finally {
    Pop-Location
}
