The password never reaches Lua: a [secure field](../guide/input.md#secure-fields) with
`secure_submit = { capability = "polkit", action = "authenticate" }` sends it straight to the
Supervisor.

```lua
column {
    visible = mantle.polkit:map(function(polkit)
        return polkit ~= nil and polkit.active
    end),
    spacing = 8,
    children = {
        text {
            content = mantle.polkit:map(function(polkit)
                return polkit and polkit.message or ""
            end),
        },
        textfield {
            width = 280,
            height = 24,
            placeholder = "Password",
            secure_submit = { capability = "polkit", action = "authenticate" },
        },
        text {
            foreground = "#F38BA8",
            content = mantle.polkit:map(function(polkit)
                return polkit and polkit.error or ""
            end),
        },
        button {
            on_click = function() mantle.polkit:cancel() end,
            children = { text { content = "Cancel" } },
        },
    },
}
```

<!-- reference -->

## Backend

On first read, registers as the authentication agent for `$XDG_SESSION_ID`'s session, at
`/org/mantle/PolicyKit1/AuthenticationAgent` with locale `en_US.UTF-8`. If another agent already
answers, it stays off for the run. polkitd accepts an answer only from uid 0, so the
[PAM worker](../../supervisor/src/pam_worker.rs) hands the password to polkit's root helper at
`/run/polkit/agent-helper.socket`. The agent is [`polkit.rs`](../../supervisor/src/polkit.rs).

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A second program's prompt never shows | One request at a time: a request arriving while another is on screen is cancelled, and its program sees the cancel. Finish or `cancel` the first |
| Prompts go to another agent | polkit-gnome, hyprpolkitagent or similar registered first. Stop it and restart Mantle |
| The prompt stays open after a wrong password | By design: `error` says why, and the next submit retries |

See also: [secure fields](../guide/input.md#secure-fields); [FAQ](../guide/faq.md#capabilities) for prompts that never appear.
