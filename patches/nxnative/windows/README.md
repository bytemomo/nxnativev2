Patches applied for the Windows build only (after common/):

- 0001-windows.patch: the NSP script's prerequisites on Windows (no bash, make or wine: the Metal compiler runs natively through the bundle's
  xcrun.exe), and the shader translator's launcher as copies of its dlls plus the bundle's dotnet-dll-launcher.exe (no symlinks or shell scripts).

Everything else the Windows build needs is in common/ (build_ipa_portable.py and gen_build_portable.py replace bash and make on every platform
but macOS) and in the bundle (crates/launchers: xcrun.exe, dotnet-dll-launcher.exe).
