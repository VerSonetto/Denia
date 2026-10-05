# The same quality gate runs locally and in CI. It never installs or starts the app.
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$TaskRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $TaskRoot
try {
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Rust 格式检查失败' }
    cargo clippy --workspace --all-targets --locked -- -D clippy::correctness -D clippy::suspicious
    if ($LASTEXITCODE -ne 0) { throw 'Rust 静态检查失败' }
    cargo run -p denia-core --features bindings --example bindings --locked -- --check
    if ($LASTEXITCODE -ne 0) { throw '前后端协议类型已过期' }
    cargo test --workspace --locked --no-fail-fast
    if ($LASTEXITCODE -ne 0) { throw 'Rust 测试失败' }
    Push-Location web
    try {
        if ($SkipBuild) {
            pnpm check
            if ($LASTEXITCODE -ne 0) { throw '前端测试失败' }
            pnpm typecheck
        } else { pnpm build }
        if ($LASTEXITCODE -ne 0) { throw '前端类型检查或构建失败' }
    } finally { Pop-Location }
} finally { Pop-Location }
