@echo off
rem Removes the sensor service that this portable copy of Monitor installed. The PawnIO
rem driver stays, as other apps can share it. Remove it from Installed apps in Windows
rem Settings if you don't need it.
setlocal
set "setup=%~dp0monitor-setup.exe"
if not exist "%setup%" (
    echo monitor-setup.exe is missing. Use Installed apps in Windows Settings instead:
    echo look for "Monitor CPU sensors".
    pause
    exit /b 1
)

echo Removing the Monitor sensor service.
echo Windows will ask for admin rights.
echo.
start "" /wait "%setup%" uninstall
set "code=%errorlevel%"

if "%code%"=="0" (
    echo Done. If you installed Monitor with the MSI, its service stays until you uninstall that.
) else if "%code%"=="13" (
    echo The sensor service couldn't be removed. See the setup log in the data folder.
) else (
    echo Nothing was removed. If you saw the admin prompt, it may have been declined. Code %code%.
)
echo.
pause
exit /b %code%
