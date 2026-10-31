@echo off
cd /d "%~dp0"
set R=report.txt
echo ferrox test report > %R%
echo date: %DATE% %TIME% >> %R%
ver >> %R%
echo cpu: %PROCESSOR_IDENTIFIER% >> %R%
echo logical processors: %NUMBER_OF_PROCESSORS% >> %R%
echo. >> %R%

echo [1/6] Collecting machine details (a few seconds)...
echo === 1. machine === >> %R%
systeminfo 2>nul | findstr /i /c:"OS Name" /c:"OS Version" /c:"Total Physical Memory" /c:"Virtualization" /c:"Hyper-V" /c:"Hypervisor" >> %R%
echo. >> %R%

echo [2/6] Built-in checks on the test interpreter...
echo === 2. self-test, test interpreter, disc image, drive image, error handling === >> %R%
ferrox.exe selftest --no-hardware selftest-output >> %R% 2>&1
echo exit code: %ERRORLEVEL% >> %R%
echo. >> %R%

echo [3/6] Built-in checks on the fast CPU (Windows Hypervisor Platform)...
echo === 3. self-test, fast CPU === >> %R%
ferrox.exe selftest --hardware selftest-output >> %R% 2>&1
set HW=%ERRORLEVEL%
echo exit code: %HW% >> %R%
if not "%HW%"=="0" call :trace
echo. >> %R%

echo [4/6] Running the demo the way the command line does...
echo === 4. command line: make the demo, run it, save the picture === >> %R%
ferrox.exe demo games >> %R% 2>&1
ferrox.exe run games\Demo\default.xbe --save-frame picture-hardware.png >> %R% 2>&1
echo exit code: %ERRORLEVEL% >> %R%
echo. >> %R%

echo [5/6] Opening the app window and playing the demo (it closes by itself)...
echo === 5. app window === >> %R%
if exist gui-smoke rmdir /s /q gui-smoke
mkdir gui-smoke
start "ferrox" /wait ferrox-gui.exe --smoke gui-smoke
echo exit code: %ERRORLEVEL% >> %R%
if exist gui-smoke\gui-smoke.txt (type gui-smoke\gui-smoke.txt >> %R%) else (echo NO gui-smoke.txt was written: the window did not open >> %R%)
echo. >> %R%

echo [6/6] Making a blank hard-drive image (4 GiB, sparse)...
echo === 6. blank hard-drive image === >> %R%
if exist test-hdd.img del test-hdd.img
ferrox.exe hdd test-hdd.img 4 >> %R% 2>&1
echo. >> %R%

echo === SUMMARY === >> %R%
findstr /b /c:"selftest:" %R% >> %R%.tmp
findstr /b /c:"FAIL" %R% >> %R%.tmp
findstr /b /c:"result:" %R% >> %R%.tmp
type %R%.tmp >> %R%
del %R%.tmp
type %R%
echo.
echo ------------------------------------------------------------
echo Done. Please send me report.txt (that is all I need).
echo To use the app: run ferrox-gui.exe, add the "games" folder, press Launch.
echo ------------------------------------------------------------
pause
exit /b

:trace
echo === 3b. same again with a detailed trace, because step 3 did not pass === >> %R%
set FERROX_TRACE=1
ferrox.exe selftest --hardware selftest-output >> %R% 2>&1
echo exit code: %ERRORLEVEL% >> %R%
set FERROX_TRACE=
exit /b
