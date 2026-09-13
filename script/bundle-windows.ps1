[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help,
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][string]$Name
)

. "$PSScriptRoot/lib/workspace.ps1"

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) {
    $Architecture
} else {
    $OSArchitecture
}

$CargoOutDir = "./target/$Architecture-pc-windows-msvc/release"

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

Push-Location
# Whichever Visual Studio install carries the MSVC toolchain: Build Tools and
# newer versions do not live where Community 2022 does.
$vsInstallPath = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
& "$vsInstallPath\Common7\Tools\Launch-VsDevShell.ps1" -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
Pop-Location

$target = "$Architecture-pc-windows-msvc"

if ($Help) {
    Write-Output "Usage: bundle-windows.ps1 [-Install] [-Help]"
    Write-Output "Build the installer for Windows.\n"
    Write-Output "Options:"
    Write-Output "  -Architecture, -a Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -Help, -h         Show this help message."
    Write-Output ""
    Write-Output "KNIGHTCODE_ENGINE_DIR must name the directory 'bun run build:engine' writes"
    Write-Output "(packages/cli-win32-x64/bin in the KnightCode repository)."
    exit 0
}

Push-Location -Path crates/zed
$channel = Get-Content "RELEASE_CHANNEL"
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

# Marks the build as installed, as bundle-mac and bundle-linux do: the updater
# starts only in a build compiled with ZED_BUNDLE.
$env:ZED_BUNDLE = "true"

function CheckEnvironmentVariables {
    if(-not $env:CI) {
        return
    }

    $requiredVars = @('ZED_WORKSPACE', 'RELEASE_VERSION', 'ZED_RELEASE_CHANNEL')

    foreach ($var in $requiredVars) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($var))) {
            Write-Error "$var is not set"
            exit 1
        }
    }
}

function PrepareForBundle {
    if (Test-Path "$innoDir") {
        Remove-Item -Path "$innoDir" -Recurse -Force
    }
    New-Item -Path "$innoDir" -ItemType Directory -Force
    Copy-Item -Path "$env:ZED_WORKSPACE\crates\zed\resources\windows\*" -Destination "$innoDir" -Recurse -Force
    # The licence page shows the licence the IDE ships under.
    Copy-Item -Path "$env:ZED_WORKSPACE\LICENSE-GPL" -Destination "$innoDir\license.txt" -Force
    New-Item -Path "$innoDir\bin" -ItemType Directory -Force

    rustup target add $target
}

function StageEngine {
    $source = $env:KNIGHTCODE_ENGINE_DIR
    if ([string]::IsNullOrWhiteSpace($source)) {
        throw "KNIGHTCODE_ENGINE_DIR is not set. It must be an absolute path to the directory 'bun run build:engine' writes: knightcode-engine.exe and the runtime assets beside it (packages/cli-win32-x64/bin in the KnightCode repository)."
    }
    if (-not (Test-Path (Join-Path $source "knightcode-engine.exe"))) {
        throw "knightcode-engine.exe was not found in $source. Build it with 'bun run build:engine'."
    }
    $dest = New-Item -Path "$innoDir\engine" -ItemType Directory -Force
    # Everything the engine reads from beside itself: package.json (its
    # version), themes, assets, export-html, docs, native prebuilds and the
    # photon wasm. The CLI binary is the one thing there the IDE does not ship.
    Get-ChildItem -Path $source | Where-Object { $_.Name -ne "knightcode.exe" } | Copy-Item -Destination $dest -Recurse -Force
    Write-Output "Staged the engine from $source"
}

function GenerateLicenses {
    . $PSScriptRoot/generate-licenses.ps1
}

function BuildZedAndItsFriends {
    Write-Output "Building KnightCode and its friends, for channel: $channel"
    # Build zed.exe, cli.exe and auto_update_helper.exe
    cargo --config .cargo/bundle-config.toml build --release --package zed --package cli --package auto_update_helper --target $target
    Copy-Item -Path ".\$CargoOutDir\zed.exe" -Destination "$innoDir\KnightCode.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
    Copy-Item -Path ".\$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
}

function ZipZedAndItsFriendsDebug {
    $items = @(
        ".\$CargoOutDir\zed.pdb",
        ".\$CargoOutDir\cli.pdb"
    )

    Compress-Archive -Path $items -DestinationPath ".\$CargoOutDir\zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip" -Force
}

function DownloadAMDGpuServices {
    # If you update the AGS SDK version, please also update the version in `crates/gpui/src/platform/windows/directx_renderer.rs`
    $url = "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0"
    $zipPath = ".\AGS_SDK_v6.3.0.zip"
    # Download the AGS SDK zip file
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    # Extract the AGS SDK zip file
    Expand-Archive -Path $zipPath -DestinationPath "." -Force
}

function DownloadConpty {
    $url = "https://github.com/microsoft/terminal/releases/download/v1.23.13503.0/Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    $zipPath = ".\Microsoft.Windows.Console.ConPTY.1.23.251216003.nupkg"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath ".\conpty" -Force
}

function CollectFiles {
    # The PATH launcher is knightcode-ide: the KnightCode CLI already owns
    # knightcode on PATH.
    Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\knightcode-ide.exe" -Force
    Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\knightcode-ide" -Force
    New-Item -Type Directory -Path "$innoDir\tools" -Force
    Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force
    if($Architecture -eq "aarch64") {
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
    else {
        New-Item -Type Directory -Path "$innoDir\x64" -Force
        New-Item -Type Directory -Path "$innoDir\arm64" -Force
        Move-Item -Path ".\AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll" -Destination "$innoDir\amd_ags_x64.dll" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
        Move-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
}

function BuildInstaller {
    $issFilePath = "$innoDir\zed.iss"
    switch ($channel) {
        "stable" {
            $appId = "{{11325453-1811-4EA4-AFF1-699B534B844C}"
            $appIconName = "app-icon"
            $appName = "KnightCode"
            $appDisplayName = "KnightCode"
            $appSetupName = "KnightCode-$Architecture"
            # Matches release_channel::app_identifier(), which names the mutex
            # in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "KnightCode-Stable-Instance-Mutex"
            $appExeName = "KnightCode"
            $regValueName = "KnightCode"
            $appUserId = "KnightCodeAI.KnightCode"
            $appShellNameShort = "K&nightCode"
        }
        "preview" {
            $appId = "{{92DC5C4B-6E17-4FBA-8B04-9A4A08F50B02}"
            $appIconName = "app-icon-preview"
            $appName = "KnightCode Preview"
            $appDisplayName = "KnightCode Preview"
            $appSetupName = "KnightCode-$Architecture"
            # Matches release_channel::app_identifier(), which names the mutex
            # in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "KnightCode-Preview-Instance-Mutex"
            $appExeName = "KnightCode"
            $regValueName = "KnightCodePreview"
            $appUserId = "KnightCodeAI.KnightCode.Preview"
            $appShellNameShort = "K&nightCode Preview"
        }
        "nightly" {
            $appId = "{{C62AB0ED-FF36-4EA2-882C-ADFF2270711C}"
            $appIconName = "app-icon-nightly"
            $appName = "KnightCode Nightly"
            $appDisplayName = "KnightCode Nightly"
            $appSetupName = "KnightCode-$Architecture"
            # Matches release_channel::app_identifier(), which names the mutex
            # in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "KnightCode-Nightly-Instance-Mutex"
            $appExeName = "KnightCode"
            $regValueName = "KnightCodeNightly"
            $appUserId = "KnightCodeAI.KnightCode.Nightly"
            $appShellNameShort = "K&nightCode Nightly"
        }
        "dev" {
            $appId = "{{8DB6F53D-FFCC-4AE0-AD87-25FD1BCD143D}"
            $appIconName = "app-icon-dev"
            $appName = "KnightCode Dev"
            $appDisplayName = "KnightCode Dev"
            $appSetupName = "KnightCode-$Architecture"
            # Matches release_channel::app_identifier(), which names the mutex
            # in crates\zed\src\zed\windows_only_instance.rs
            $appMutex = "KnightCode-Dev-Instance-Mutex"
            $appExeName = "KnightCode"
            $regValueName = "KnightCodeDev"
            $appUserId = "KnightCodeAI.KnightCode.Dev"
            $appShellNameShort = "K&nightCode Dev"
        }
        default {
            Write-Error "can't bundle installer for $channel."
            exit 1
        }
    }

    # CI images install Inno Setup per machine; winget, run unelevated,
    # installs it per user.
    $innoSetupPath = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $innoSetupPath) {
        throw "ISCC.exe was not found. Install Inno Setup 6: winget install --exact --id JRSoftware.InnoSetup"
    }

    $definitions = @{
        "AppId"          = $appId
        "AppIconName"    = $appIconName
        "OutputDir"      = "$env:ZED_WORKSPACE\target"
        "AppSetupName"   = $appSetupName
        "AppName"        = $appName
        "AppDisplayName" = $appDisplayName
        "RegValueName"   = $regValueName
        "AppMutex"       = $appMutex
        "AppExeName"     = $appExeName
        "ResourcesDir"   = "$innoDir"
        "ShellNameShort" = $appShellNameShort
        "AppUserId"      = $appUserId
        "Version"        = "$env:RELEASE_VERSION"
        "SourceDir"      = "$env:ZED_WORKSPACE"
    }

    $defs = @()
    foreach ($key in $definitions.Keys) {
        $defs += "/d$key=`"$($definitions[$key])`""
    }

    $innoArgs = @($issFilePath) + $defs

    # Execute Inno Setup
    Write-Host "Running Inno Setup: $innoSetupPath $innoArgs"
    $process = Start-Process -FilePath $innoSetupPath -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru

    if ($process.ExitCode -eq 0) {
        Write-Host "Inno Setup compiled the installer"
        Write-Output "SETUP_PATH=target/$appSetupName.exe" >> $env:GITHUB_ENV
        $script:buildSuccess = $true
    }
    else {
        Write-Host "Inno Setup failed: $($process.ExitCode)"
        $script:buildSuccess = $false
    }
}

ParseZedWorkspace
$innoDir = "$env:ZED_WORKSPACE\inno\$Architecture"
$debugArchive = "$CargoOutDir\zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip"
$debugStoreKey = "$env:ZED_RELEASE_CHANNEL/zed-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip"

# Not built, on purpose:
# - remote_server: a separate archive for SSH remoting, with no release feed
#   to serve it from.
# - explorer_command_injector and its appx: the package claims Zed Industries'
#   identity, and installing it needs a trusted signature. The classic
#   context-menu entries in zed.iss cover Windows 11 under "Show more options".
# - code signing: there is no certificate. sign.ps1 stays in the tree, so
#   signing is one function again the day there is one.
# - the Sentry symbol upload: it targets Zed's Sentry organisation.
CheckEnvironmentVariables
PrepareForBundle
StageEngine
GenerateLicenses
BuildZedAndItsFriends
ZipZedAndItsFriendsDebug
DownloadAMDGpuServices
DownloadConpty
CollectFiles
BuildInstaller

if ($buildSuccess) {
    Write-Output "Build successful"
    if ($Install) {
        Write-Output "Installing KnightCode..."
        Start-Process -FilePath "$env:ZED_WORKSPACE/target/KnightCode-$Architecture.exe"
    }
    exit 0
}
else {
    Write-Output "Build failed"
    exit 1
}
