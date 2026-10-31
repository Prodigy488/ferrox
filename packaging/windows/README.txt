ferrox - early test build (Windows x64)

1. Double-click run-tests.bat. It writes report.txt and two pictures
   (picture-interpreter.png, picture-hardware.png) next to it.
2. Send report.txt back. If a picture was written, open it: it should be a
   colourful diagonal test pattern.
3. Optional: run ferrox-gui.exe, "Add folder..." -> pick the "games" folder
   created by step 1, press Launch. The window title shows the build id.

The fast CPU needs the Windows feature "Windows Hypervisor Platform"
(Turn Windows features on or off -> tick it -> restart) and virtualization
enabled in the BIOS/UEFI. Without it step 3 prints a message saying so; that is
not a bug. In the app, Settings -> "Use the slow test interpreter" still works.

Not working yet: real games. No 3D graphics, audio, controllers or file access.
