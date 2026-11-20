# Bundled ConPTY (Windows)

Official Microsoft ConPTY build from Windows Terminal, shipped next to every `sich.exe` so
terminals use it instead of the system ConPTY (which drops APC sequences such as Kitty graphics).

- Package: `Microsoft.Windows.Console.ConPTY` (authors: Microsoft, license: MIT)
- Version: 1.25.261109001
- Source: https://www.nuget.org/api/v2/package/Microsoft.Windows.Console.ConPTY/1.25.261109001
- Project: https://github.com/microsoft/terminal

| File (x64) | Package path | SHA-256 |
| --- | --- | --- |
| `x64/conpty.dll` | `runtimes/win-x64/native/conpty.dll` | `e971856f89748dc3b458125e9afdd79b5e9574a7c937676116d3cbbd3b71f492` |
| `x64/OpenConsole.exe` | `build/native/runtimes/x64/OpenConsole.exe` | `b17e48b07ef153b420f0a6e178bacd31fb4c3390e21ae746586213fc0719a775` |

To update: download the new package into an empty directory, copy the two files, update the
version and hashes here.
