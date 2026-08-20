# Notifications

Craft can tell you when a session finishes or needs your input. This is useful when you move to another terminal while Craft works.

Notifications are enabled by default. Craft does not notify while it knows that its terminal has focus.

Craft uses these messages:

- `Agent turn complete`, or a preview of the response up to 200 characters.
- `Permission requested: <tool>` for a permission prompt.
- `Authentication required` when authentication needs attention.
- `Question requested` for a question prompt.
- `Plan ready` when a plan is ready.

Response previews can appear in your operating system's notification history. Craft does not include tool arguments, permission scopes, question bodies, plan content, or error details. Use `bell` for a message-free alert, or use `off` to disable notifications if response text should not reach notification history.

## Configuration

Set `ui.notifications` in `~/.config/craft/init.lua`:

```lua
craft.setup({
  ui = {
    notifications = "auto",
  },
})
```

| Value | Behavior |
| --- | --- |
| `auto` | Use OSC 9 in a supported terminal. Use BEL otherwise. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `off` | Do not send notifications. |

`auto` supports Ghostty, iTerm2, Kitty, Warp, and WezTerm. An unknown terminal uses BEL. Your terminal settings decide whether BEL makes a sound or shows a visual alert.

Craft also recognizes `xterm-ghostty` and `xterm-kitty` from `TERM`. This lets OSC 9 work when an SSH connection does not preserve `TERM_PROGRAM`.

## tmux

tmux needs both of these settings:

```tmux
set -g focus-events on
set -g allow-passthrough all
```

Use `allow-passthrough all`, not `allow-passthrough on`. The `on` value permits passthrough only while the Craft pane is visible. tmux drops the notification after you change to another tmux window.

Add the settings to `~/.tmux.conf`, then reload the file or restart tmux.

## Other terminal multiplexers

Craft wraps OSC 9 for GNU screen. GNU screen does not pass terminal focus events to Craft, so Craft does not suppress notifications there. A notification can appear while the GNU screen window has focus.

Craft sends OSC 9 directly through Zellij.

## Focus on Windows

The terminal focus protocol is not available on Windows. Craft treats the terminal as unfocused, so an explicit `bell` or `osc9` setting still works.
