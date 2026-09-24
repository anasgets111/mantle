The password goes through a `secure_submit` [text field](../guide/input.md#secure-fields), never through Lua.

```lua
text {
    visible = mantle.polkit:map(function(polkit)
        return polkit ~= nil and polkit.active
    end),
    content = mantle.polkit:map(function(polkit)
        return polkit and polkit.message or ""
    end),
}
```

<!-- reference -->

## Backend

Registers as the authentication agent for `$XDG_SESSION_ID`'s session at
`/org/mantle/PolicyKit1/AuthenticationAgent` on its first start; if another agent already answers,
it stays off for the run. Challenge state and cancel belong to the Supervisor. The
[PAM worker](../../supervisor/src/pam_worker.rs) passes the password from the secure field to
polkit's root helper at `/run/polkit/agent-helper.socket`; polkitd accepts the response only from
uid 0. The agent is [`polkit.rs`](../../supervisor/src/polkit.rs).
