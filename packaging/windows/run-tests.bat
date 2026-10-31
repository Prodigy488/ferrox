@echo off
cd /d "%~dp0"
set R=report.txt
echo ferrox test report > %R%
echo date: %DATE% %TIME% >> %R%
ver >> %R%
echo. >> %R%
echo === 1. make the demo game === >> %R%
ferrox.exe demo games >> %R% 2>&1
echo. >> %R%
echo === 2. run it on the slow test interpreter === >> %R%
ferrox.exe run games\Demo\default.xbe --reference-cpu --save-frame picture-interpreter.png >> %R% 2>&1
echo exit code: %ERRORLEVEL% >> %R%
echo. >> %R%
echo === 3. run it on the Windows Hypervisor Platform (fast CPU) === >> %R%
set FERROX_TRACE=1
ferrox.exe run games\Demo\default.xbe --save-frame picture-hardware.png >> %R% 2>&1
echo exit code: %ERRORLEVEL% >> %R%
set FERROX_TRACE=
echo. >> %R%
echo === 4. make a blank hard-drive image (4 GiB, sparse) === >> %R%
if exist test-hdd.img del test-hdd.img
ferrox.exe hdd test-hdd.img 4 >> %R% 2>&1
echo. >> %R%
type %R%
echo.
echo ------------------------------------------------------------
echo Done. Please send me report.txt (and look at the two .png files).
echo To use the app: run ferrox-gui.exe, add the "games" folder, press Launch.
echo ------------------------------------------------------------
pause
