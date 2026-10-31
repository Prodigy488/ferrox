ferrox - early test build (Windows x64)

1. Double-click run-tests.bat. It runs seven groups of checks (about a minute and a half;
   an app window opens briefly by itself and closes) and writes report.txt.
   Step 6 tries your game for 30 seconds. By default it looks for
   D:\Xemu\Tony Hawk's Pro Skater 2x (USA) (a folder, an .iso/.xiso, or a
   name with the extension left off all work). To use another game, either
   put its path on the first line of a file called game.txt next to
   run-tests.bat, or drag the game file/folder onto run-tests.bat.
2. Send report.txt back (and game-frame.png if it exists). That is all that is needed: it ends with a SUMMARY
   that lists every check that failed.
3. Optional: run ferrox-gui.exe, "Add folder..." -> pick the "games" folder
   created by step 1, press Launch. The window title shows the build id.
   Pictures from the checks are in selftest-output and gui-smoke.

The fast CPU needs the Windows feature "Windows Hypervisor Platform"
(Turn Windows features on or off -> tick it -> restart) and virtualization
enabled in the BIOS/UEFI. Without it the fast-CPU checks are skipped with a
message saying so; that is not a bug. In the app, Settings -> "Use the slow
test interpreter" still works.

If the app window stays open for more than half a minute, close it by hand;
the script continues.

Not working yet: real games (step 6 shows how far one gets and what is missing). 3D graphics are partly done (drawing, depth,
textures); no audio, controllers or file access.
