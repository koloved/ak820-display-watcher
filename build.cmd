@echo off
rem Сборка требует окружения Visual Studio: без него компилятор C-библиотеки
rem hidapi падает с ошибкой D8050. Профиль release не добавляет -Z7, поэтому debug
rem в этой системе не собирается — используйте release.
setlocal
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
    for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -property installationPath`) do set "VSDIR=%%i"
)
if defined VSDIR (
    call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
) else (
    echo Не найден Visual Studio, пробую собрать как есть...
)
cargo build --release
endlocal
