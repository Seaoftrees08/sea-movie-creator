@echo off
setlocal EnableExtensions EnableDelayedExpansion

for %%I in ("%~dp0..") do set "REPO_ROOT=%%~fI"

set "SKIP_BUILD=0"
set "CREATE_ZIP=0"
set "PKG_VERSION="

:parse_args
if "%~1"=="" goto args_done
if /i "%~1"=="--skip-build" (
    set "SKIP_BUILD=1"
    shift /1
    goto parse_args
)
if /i "%~1"=="--zip" (
    set "CREATE_ZIP=1"
    shift /1
    goto parse_args
)
if /i "%~1"=="--version" (
    if "%~2"=="" (
        echo [ERROR] --version requires a value.
        exit /b 1
    )
    set "PKG_VERSION=%~2"
    shift /1
    shift /1
    goto parse_args
)
if /i "%~1"=="--help" goto show_help
if /i "%~1"=="-h" goto show_help
echo [ERROR] Unknown option: %~1
echo.
goto show_help_error

:args_done
if not exist "%REPO_ROOT%\Cargo.toml" (
    echo [ERROR] Cargo.toml not found under repository root: %REPO_ROOT%
    exit /b 1
)

if defined PKG_VERSION (
    if /i "!PKG_VERSION:~0,1!"=="v" set "PKG_VERSION=!PKG_VERSION:~1!"
) else (
    set "IN_PACKAGE=0"
    for /f "usebackq tokens=* delims=" %%L in ("%REPO_ROOT%\Cargo.toml") do (
        set "LINE=%%L"
        set "TRIMMED=!LINE: =!"
        if "!TRIMMED!"=="[package]" (
            set "IN_PACKAGE=1"
        ) else if "!LINE:~0,1!"=="[" (
            set "IN_PACKAGE=0"
        ) else if "!IN_PACKAGE!"=="1" (
            for /f "tokens=1,* delims==" %%A in ("!LINE!") do (
                set "KEY=%%A"
                set "KEY=!KEY: =!"
                set "KEY=!KEY:	=!"
                if "!KEY!"=="version" (
                    set "VAL=%%B"
                    for /f "tokens=1 delims=#" %%C in ("!VAL!") do set "VAL=%%C"
                    set "VAL=!VAL: =!"
                    set "VAL=!VAL:	=!"
                    set "VAL=!VAL:"=!"
                    if not defined PKG_VERSION set "PKG_VERSION=!VAL!"
                )
            )
        )
    )
)

if not defined PKG_VERSION (
    echo [ERROR] Failed to determine package version from Cargo.toml.
    exit /b 1
)

pushd "%REPO_ROOT%" || exit /b 1

echo ==^> Packaging sea-movie-creator v%PKG_VERSION% ^(Windows x86_64^)
echo ==^> Repository root: %REPO_ROOT%

if "%SKIP_BUILD%"=="0" (
    echo [1/4] Building release workspace ^(cargo build --release --workspace^)...
    cargo build --release --workspace
    if errorlevel 1 (
        echo [ERROR] Release build failed.
        goto fail
    )
) else (
    echo [1/4] Skipping cargo build ^(--skip-build^).
)

set "PKG_DIR_NAME=sea-movie-creator-v%PKG_VERSION%-alpha-windows-x86_64"
set "DIST_DIR=%REPO_ROOT%\dist\%PKG_DIR_NAME%"
set "TARGET_REL=%REPO_ROOT%\target\release"

echo [2/4] Preparing clean distribution directory: %DIST_DIR%
if exist "%DIST_DIR%" (
    rmdir /s /q "%DIST_DIR%"
    if errorlevel 1 (
        echo [ERROR] Failed to remove existing directory: %DIST_DIR%
        goto fail
    )
)
if not exist "%REPO_ROOT%\dist" (
    mkdir "%REPO_ROOT%\dist" || goto fail
)
mkdir "%DIST_DIR%" || goto fail
mkdir "%DIST_DIR%\lang" || goto fail
mkdir "%DIST_DIR%\plugins\audio" || goto fail
mkdir "%DIST_DIR%\plugins\export" || goto fail
mkdir "%DIST_DIR%\plugins\ai" || goto fail
mkdir "%DIST_DIR%\plugins\agent" || goto fail

echo [3/4] Copying binaries, manifests, localization, and documentation...
call :copy_required "%TARGET_REL%\sea-movie-creator.exe" "%DIST_DIR%\sea-movie-creator.exe" || goto fail
call :copy_required "%REPO_ROOT%\LICENSE-MIT" "%DIST_DIR%\LICENSE-MIT" || goto fail
call :copy_required "%REPO_ROOT%\LICENSE-APACHE" "%DIST_DIR%\LICENSE-APACHE" || goto fail
call :copy_required "%REPO_ROOT%\README.md" "%DIST_DIR%\README.md" || goto fail

if not exist "%REPO_ROOT%\docs" (
    echo [ERROR] Missing docs directory: %REPO_ROOT%\docs
    goto fail
)
xcopy "%REPO_ROOT%\docs" "%DIST_DIR%\docs\" /e /i /y /q >nul
if errorlevel 1 (
    echo [ERROR] Failed to copy docs directory.
    goto fail
)

set /a LANG_COUNT=0
for %%F in ("%REPO_ROOT%\lang\*.json") do (
    copy /y "%%~fF" "%DIST_DIR%\lang\%%~nxF" >nul
    if errorlevel 1 (
        echo [ERROR] Failed to copy localization file: %%~fF
        goto fail
    )
    set /a LANG_COUNT+=1
)
if !LANG_COUNT! EQU 0 (
    echo [ERROR] No localization JSON files found under %REPO_ROOT%\lang
    goto fail
)

call :copy_required "%TARGET_REL%\sea-audio-host.exe" "%DIST_DIR%\plugins\audio\sea-audio-host.exe" || goto fail
call :copy_plugin_group export plugin.json || goto fail
call :copy_plugin_group ai ai-plugin.json || goto fail
call :copy_plugin_group agent agent-plugin.json || goto fail

if exist "%DIST_DIR%\settings" (
    echo [ERROR] Unexpected settings directory detected in distribution output.
    goto fail
)

if "%CREATE_ZIP%"=="1" (
    set "ZIP_PATH=%DIST_DIR%.zip"
    if exist "!ZIP_PATH!" del /f /q "!ZIP_PATH!"
    echo [4/4] Creating ZIP archive: !ZIP_PATH!
    tar.exe -a -c -f "!ZIP_PATH!" -C "%REPO_ROOT%\dist" "%PKG_DIR_NAME%"
    if errorlevel 1 (
        echo [ERROR] Failed to create ZIP archive with tar.exe.
        goto fail
    )
) else (
    echo [4/4] Skipping ZIP creation ^(pass --zip to also generate .zip^).
)

echo.
echo [OK] Release package ready:
echo      Directory: %DIST_DIR%
if "%CREATE_ZIP%"=="1" (
    echo      Archive:   %DIST_DIR%.zip
)
popd
exit /b 0

:copy_required
set "SRC_FILE=%~1"
set "DST_FILE=%~2"
if not exist "%SRC_FILE%" (
    echo [ERROR] Required file not found: %SRC_FILE%
    exit /b 1
)
copy /y "%SRC_FILE%" "%DST_FILE%" >nul
if errorlevel 1 (
    echo [ERROR] Failed to copy %SRC_FILE% to %DST_FILE%
    exit /b 1
)
exit /b 0

:copy_plugin_group
set "GRP=%~1"
set "MANIFEST_NAME=%~2"
set /a GRP_COUNT=0
if not exist "%REPO_ROOT%\plugins\%GRP%" (
    echo [ERROR] Missing plugins directory: %REPO_ROOT%\plugins\%GRP%
    exit /b 1
)
for /d %%D in ("%REPO_ROOT%\plugins\%GRP%\*") do (
    set "PLUGIN_SUB=%%~nxD"
    set "MANIFEST_SRC=%%~fD\%MANIFEST_NAME%"
    if exist "!MANIFEST_SRC!" (
        call :extract_entrypoint "!MANIFEST_SRC!" ENTRYPOINT
        if errorlevel 1 exit /b 1
        if /i "!ENTRYPOINT:~-4!"==".exe" set "ENTRYPOINT=!ENTRYPOINT:~0,-4!"
        set "DST_SUB=%DIST_DIR%\plugins\%GRP%\!PLUGIN_SUB!"
        mkdir "!DST_SUB!" || exit /b 1
        copy /y "!MANIFEST_SRC!" "!DST_SUB!\%MANIFEST_NAME%" >nul || exit /b 1
        call :copy_required "%TARGET_REL%\!ENTRYPOINT!.exe" "!DST_SUB!\!ENTRYPOINT!.exe" || exit /b 1
        set /a GRP_COUNT+=1
    )
)
if !GRP_COUNT! EQU 0 (
    echo [ERROR] No plugins found in %REPO_ROOT%\plugins\%GRP%
    exit /b 1
)
exit /b 0

:extract_entrypoint
set "EP_FILE=%~1"
set "EP_OUTVAR=%~2"
set "EP_VAL="
for /f "usebackq tokens=1,* delims=:" %%A in (`findstr /r /c:"\"entrypoint\"" "%EP_FILE%"`) do (
    if not defined EP_VAL (
        set "RAW_EP=%%B"
        set "RAW_EP=!RAW_EP:"=!"
        set "RAW_EP=!RAW_EP:,=!"
        set "RAW_EP=!RAW_EP: =!"
        set "RAW_EP=!RAW_EP:	=!"
        set "EP_VAL=!RAW_EP!"
    )
)
if not defined EP_VAL (
    echo [ERROR] Could not extract entrypoint from %EP_FILE%
    exit /b 1
)
set "%EP_OUTVAR%=%EP_VAL%"
exit /b 0

:show_help
echo Usage: scripts\package-release.bat [options]
echo.
echo Options:
echo   --skip-build       Skip "cargo build --release --workspace" and package existing target\release
echo   --zip              Also create dist\sea-movie-creator-v^<ver^>-alpha-windows-x86_64.zip
echo   --version ^<ver^>    Override package version ^(default: read from Cargo.toml [package]^)
echo   -h, --help         Show this help message
exit /b 0

:show_help_error
echo Usage: scripts\package-release.bat [--skip-build] [--zip] [--version ^<ver^>] [-h^|--help]
exit /b 1

:fail
popd
exit /b 1
