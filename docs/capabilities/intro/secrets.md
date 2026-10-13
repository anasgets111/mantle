A `secure_submit` field can store a named UTF-8 value in the session Secret Service. The Renderer
sends the native buffer straight to the Supervisor; callbacks, Lua state, logs, and command arguments
see no value bytes. The service stores it under schema `io.github.anasgets111.mantle.Secret` with
attribute `name`. Another application must query that schema and attribute to find it.

```lua
column {
    children = {
        textfield {
            width = 240, height = 36,
            secure_submit = { capability = "secrets", action = "store", name = "mail" },
        },
        text {
            content = mantle.secrets:map(function(secrets)
                return secrets.entries.mail or ""
            end, ""),
        },
    },
}
```

Names are public Secret Service attributes. Use a short label, not account credentials or any other
secret. Names must be nonblank UTF-8 without control characters, at most 128 bytes. Repeating a
write to a name while one is pending is ignored. With no Secret Service installed (gnome-keyring,
KeePassXC or oo7-daemon), the state reports `unavailable`; Mantle does not write a fallback file. A
locked keyring shows the keyring's own unlock prompt, and a missing default keyring is created the
same way. The store waits as long as the user takes to answer the prompt; a keyring that exits
meanwhile reports `unavailable`. A service call that takes over 25 seconds reports `timed_out`,
which is final, and the name accepts a new write. The state describes the latest write in this Supervisor session and does not list or read
stored values.
