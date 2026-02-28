Set FontFamily "Menlo"
Set FontSize 28
Set Width 1520
Set Height 856
Set Padding 0
Set Margin 0
Set BorderRadius 0
Set TypingSpeed 50ms
Output demo/demo.gif

# Launch mtack before recording starts
Hide
Type "cargo run -- -c demo/mtack.kdl"
Enter
Sleep 4
Show

Sleep 1.5s

# Tab to build
Type "l"
Sleep 1.5s

# Tab to python
Type "l"
Sleep 0.8s

# Focus mode + type into REPL
Type "i"
Sleep 0.4s
Type "print('hello from mtack!')"
Enter
Sleep 1s
Escape
Sleep 0.5s

# Jump back to server
Type "1"
Sleep 0.5s

# Search for POST
Type "/"
Sleep 0.3s
Type "POST"
Enter
Sleep 1.5s

# Clear search
Escape
Sleep 0.5s

# Switch to build, stop and restart it
Type "2"
Sleep 0.5s
Type "x"
Sleep 1s
Type "s"
Sleep 2s
