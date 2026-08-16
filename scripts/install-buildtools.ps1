# 安装 VS 2022 Build Tools（C++ 工作负载），供 Rust/MSVC 链接使用
$ErrorActionPreference = 'Stop'

Write-Output '== 检查是否已安装 MSVC =='
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path $vswhere) {
    $installed = & $vswhere -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($installed) {
        Write-Output "已安装: $installed"
        exit 0
    }
}

Write-Output '== 通过 winget 安装 BuildTools（C++ 工作负载）=='
$override = '--quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
$proc = Start-Process -FilePath 'winget' -ArgumentList @(
    'install', '--id', 'Microsoft.VisualStudio.2022.BuildTools', '-e',
    '--accept-source-agreements', '--accept-package-agreements',
    '--override', $override
) -PassThru -Wait -NoNewWindow
Write-Output "winget 退出码: $($proc.ExitCode)"

# 汇报结果
if (Test-Path $vswhere) {
    $installed = & $vswhere -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($installed) { Write-Output "MSVC 已就绪: $installed" }
    else { Write-Output '警告：未检测到 MSVC 组件' }
} else {
    Write-Output '警告：vswhere 不存在'
}
