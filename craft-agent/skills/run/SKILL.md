---
name: run
description: Build, launch, and drive the real application so changes are exercised against a running process, not just compiled. Use when asked to start, run, build, screenshot, or interact with the app
when_to_use: When the user asks to run, start, launch, build, or drive the application, or when you need a running instance to verify a change against
---

# Run the real app

Writing code is half the job. The other half is running it. If you cannot launch the app and interact with it, you have not finished. The README is never enough; the obstacles you hit while launching are content worth capturing.

## Definition of done

- Launched the real application (not just `cargo build` / `npm run build`).
- Interacted with it at its real surface (typed a command, hit a route, drove the TUI).
- Captured evidence of it running (pane capture, response body, screenshot).
- If the launch recipe was non-obvious, wrote it down (see "Persist what you learned" below).
- Every code block in this skill is a command you actually ran and that worked.

Generic troubleshooting means you did not execute. "Install dependencies and run the server" is not a run recipe; the exact flags, env vars, and readiness marker are.

## Match the project shape

Pick the driver pattern that matches the application:

| Shape | How you drive it |
|---|---|
| CLI tool | run the binary with arguments; capture stdout/stderr/exit code |
| Server / API | background-launch, readiness-poll, smoke request, clean shutdown |
| TUI / interactive terminal | wrap in tmux: send-keys, capture-pane, kill-session |
| GUI | headless runner (xvfb / Playwright); screenshot |
| Library | consume through the public package boundary, not `./src/...` |

## TUI lifecycle (tmux)

Interactive terminal apps take over the terminal and cannot be driven by a plain shell command. Wrap them in tmux so you can send input, capture output, and quit cleanly:

```bash
tmux new-session -d -s app -x 120 -y 40 './myapp'
```

Poll for a ready marker instead of a fixed sleep. It returns the instant the app is up and fails loudly if it never comes up:

```bash
timeout 10 bash -c 'until tmux capture-pane -t app -p | grep -q "Ready"; do sleep 0.2; done'
tmux capture-pane -t app -p
```

Send input:

```bash
tmux send-keys -t app 's'
timeout 5 bash -c 'until tmux capture-pane -t app -p | grep -q "Settings"; do sleep 0.2; done'
tmux capture-pane -t app -p
```

Quit cleanly, with a fallback:

```bash
tmux send-keys -t app 'q'
tmux kill-session -t app 2>/dev/null || true
```

Document the keybinding table; it is the API of a TUI. Specify a known-good terminal size in the `-x -y` args, since some TUIs break at small widths.

## Server lifecycle

A foreground command that blocks the shell is useless to an agent. Launch in the background, poll for readiness, smoke the endpoint, then shut down:

```bash
npm run dev &> /tmp/server.log &
SERVER_PID=$!

for i in {1..30}; do
  curl -sf http://localhost:3000/health > /dev/null && break
  sleep 1
done
curl http://localhost:3000/health
```

Stop cleanly:

```bash
kill $SERVER_PID
pkill -f "node.*server.js"   # fallback if you lost the PID
```

Make the port explicit. Say what "ready" looks like (a specific log line or health endpoint). Note required env vars and dependent services.

## Readiness polling, not sleep

Never `sleep N` and hope. Poll for the readiness marker:

```bash
until grep -q "Ready" /tmp/server.log; do sleep 0.2; done
```

It returns instantly when the app is up and fails loudly when it is not. A fixed sleep is a guess; a poll is a measurement.

## Persist what you learned

If the launch recipe was non-obvious (wrong documented command, hidden env var, needed a tmux wrapper), record it in a project run skill or AGENTS.md so the next session skips the cold start. Keep it short: the commands that worked, the flows worth driving, any gotchas. Do not rewrite existing docs for style; edit them only when they steered you wrong.
