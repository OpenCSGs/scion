# Records docs/demo/scion-demo.mp4 with VHS (https://github.com/charmbracelet/vhs); see README.md
Output docs/demo/scion-demo.mp4

Require tmux
Require scn

Set Shell bash
Set FontSize 14
Set Width 1920
Set Height 1080
Set Padding 12
Set Theme "Catppuccin Mocha"
Set TypingSpeed 25ms

Hide
Type "export PS1='$ ' && clear" Enter
Show
Type "# Four agents change one Scion repository at once; the right pane watches main on the server" Sleep 800ms Enter
Type "docs/demo/run.sh" Sleep 400ms Enter
Sleep 26s
Hide
Type "q"
