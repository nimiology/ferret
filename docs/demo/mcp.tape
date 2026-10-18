# vhs docs/demo/mcp.tape  -> docs/demo/mcp.gif
# Needs: claude CLI logged in, `claude mcp add --scope user ferret -- ff mcp` done.
# Agent output varies per run; re-record until the take is clean.
Output docs/demo/mcp.gif
Set Shell zsh
Set FontSize 18
Set Width 1200
Set Height 700
Set Theme "Catppuccin Mocha"

Type "claude mcp list"
Enter
Sleep 2s

Type `claude -p "Use ferret to find the 5 largest video files I changed this month"`
Enter
Sleep 25s

Type `claude -p "Where is my 2025 tax return PDF?"`
Enter
Sleep 20s
