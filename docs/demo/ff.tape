# vhs docs/demo/ff.tape  -> docs/demo/ff.gif
# Recorded against a throwaway demo home (fake files), never a real disk:
#   make the demo home at /tmp/demo/nima, `HOME=/tmp/demo/nima ff index --root /tmp/demo/nima`,
#   and run `HOME=/tmp/demo/nima ferretd --root /tmp/demo/nima` while recording.
Output docs/demo/ff.gif
Env HOME "/tmp/demo/nima"
Env PATH "/tmp/demo/bin:/opt/homebrew/bin:/usr/bin:/bin"
Set Shell zsh
Set FontSize 20
Set Width 1200
Set Height 620
Set Padding 28
Set Theme "Catppuccin Mocha"
Set TypingSpeed 55ms

Hide
Type "cd ~ && clear"
Enter
Show

Sleep 600ms
Type "ff invoice 2025 ext:pdf --limit 5"
Sleep 400ms
Enter
Sleep 2.2s

Type "ff big --limit 5"
Sleep 400ms
Enter
Sleep 2.4s

Type "ff -i"
Enter
Sleep 900ms
Type "cargo"
Sleep 1.4s
Down
Sleep 500ms
Down
Sleep 900ms
Tab
Sleep 1.2s
Tab
Sleep 1s
Escape
Sleep 600ms

Type "clear && ff '*.mov' --sort size"
Enter
Sleep 2.5s
