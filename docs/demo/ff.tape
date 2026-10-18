# vhs docs/demo/ff.tape  -> docs/demo/ff.gif
# Needs: ff on PATH, index built, daemon running.
Output docs/demo/ff.gif
Set Shell zsh
Set FontSize 18
Set Width 1200
Set Height 640
Set Theme "Catppuccin Mocha"

Type "ff invoice 2025 *.pdf"
Enter
Sleep 2s

Type "ff ext:mov size:>1GB --sort size --limit 5"
Enter
Sleep 2s

Type "ff -i"
Enter
Sleep 1s
Type "cargo toml"
Sleep 2s
Down
Down
Sleep 1s
Escape
Sleep 1s
