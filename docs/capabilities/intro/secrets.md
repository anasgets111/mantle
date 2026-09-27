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
                return secrets and secrets.entries.mail or ""
            end),
        },
    },
}
```

Names are public Secret Service attributes. Use a short label, not account credentials or any
other secret. Names must be nonblank UTF-8 without control characters, at most 128 bytes. Repeating a write to a
name while one is pending is ignored. With no usable Secret Service, the state reports
`unavailable`; Mantle does not write a fallback file. After 30 seconds, it requests cancellation
and reports `timed_out`. A stalled write still holds its name until the service returns, so a retry
cannot race it. A late result then updates the status. The state describes the latest write in this
Supervisor session and does not list or read stored values.
