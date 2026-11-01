@echo off
rem Turns on CPU temperature, frequency and power for this portable copy of Monitor.
rem The same thing happens if you click the sensor icon in the app.
setlocal
set "setup=%~dp0monitor-setup.exe"
if not exist "%setup%" (
    echo monitor-setup.exe is missing. Unpack the whole zip, then try again.
    pause
    exit /b 1
)

echo Installing the Monitor sensor service and the PawnIO driver.
echo Windows will ask for admin rights.
echo.
start "" /wait "%setup%" install
set "code=%errorlevel%"

if "%code%"=="0" (
    echo Done. Monitor can now read the CPU sensors.
) else if "%code%"=="10" (
    echo Almost done. Restart Windows to finish installing the PawnIO driver.
) else if "%code%"=="11" (
    echo PawnIO_setup.exe is missing. Unpack the whole zip, then try again.
) else if "%code%"=="12" (
    echo The PawnIO driver couldn't be installed. See the setup log in the data folder.
) else if "%code%"=="13" (
    echo The sensor service couldn't be installed. See the setup log in the data folder.
) else (
    echo Nothing was installed. If you saw the admin prompt, it may have been declined. Code %code%.
)
echo.
pause
exit /b %code%
