# mtack

A terminal multiplexer for dev workflows. Run your entire dev stack with one
command, each process in its own tab with full terminal emulation.

<img src="https://vhs.charm.sh/vhs-16FscY1PkmvLZjRcGZeJuA.gif" alt="mtack demo" width="760">

## Why mtack?

There are great tools in this space. mtack exists because none were quite
the right fit for me.

[foreman](https://github.com/ddollar/foreman),
[hivemind](https://github.com/DarthSim/hivemind), and
[concurrently](https://www.npmjs.com/package/concurrently) interleave output
into a single stream. I wanted isolated per-process output with scrollback,
search, and the ability to interact with individual processes.

[tmux](https://github.com/tmux/tmux) and
[zellij](https://github.com/zellij-org/zellij) are general-purpose
multiplexers. I wanted something project-local and declarative. A config file
in the repo that launches the whole dev stack with one command. Auto-restart
and lifecycle management built in.

[mprocs](https://github.com/pvolok/mprocs) is the closest in spirit. I wanted
horizontal tabs for maximum viewport space, full vt100 terminal emulation (so
`top` and `vim` render correctly), vi-native keybindings, and deep scrollback
with search.

[process-compose](https://github.com/F1bonacc1/process-compose) has rich
orchestration features like health checks, dependency ordering, and scheduling.
I wanted something simpler for the common case of "run my 3-5 dev servers and
tail their output."

## Install

Download a pre-built binary from
[GitHub Releases](https://github.com/zakj/mtack/releases), or build from
source:

```
cargo install --path .
```

## Quick start

Create `mtack.kdl` in your project root:

```kdl
scrollback 20000

proc "api" {
    cmd "cargo" "watch" "-x" "run"
    cwd "~/repos/api"
    env {
        RUST_LOG "debug"
    }
}

proc "frontend" {
    shell "npm run dev"
    cwd "~/repos/web"
}

proc "docker" {
    cmd "docker" "compose" "up"
    autorestart #false
}
```

Then run `mtack`. Press `?` for keybindings.

## Detaching

mtack runs your processes in a background server, and `mtack` attaches to it.
Press `d` to detach and leave everything running; press `q` to stop the
processes and quit. Closing the terminal also detaches.

| Command        | Description                                          |
|----------------|------------------------------------------------------|
| `mtack`        | Start the processes if they aren't running, attach   |
| `mtack up`     | Start the processes in the background                |
| `mtack attach` | Attach to running processes                          |
| `mtack down`   | Stop the processes and wait for them to exit         |

There is one server per config file, so running `mtack` from another terminal
takes over the view. Outside any project, `attach` and `down` use the running
server if there is only one, and list them if there are several. The server reads the config when it starts: run
`mtack down` to pick up changes. Processes without a `cwd` run in the
directory where the server started. If the server dies, its errors are
logged in `/tmp/mtack-<uid>/` (set `MTACK_TMPDIR` to use another directory
than `/tmp`).

## Config

mtack looks for `mtack.kdl` (or `.mtack.kdl`) in the current directory and
parent directories. Override with `-c <path>`.

### Global options

| Option             | Default | Description                               |
|--------------------|---------|-------------------------------------------|
| `scrollback`       | 2000    | Lines of scrollback per process           |
| `shutdown-timeout` | 5       | Seconds before SIGKILL on shutdown        |

### Process options

| Option        | Default | Description                                       |
|---------------|---------|---------------------------------------------------|
| `cmd`         |         | Command and arguments                             |
| `shell`       |         | Shell command string (passed to `$SHELL -c`)      |
| `cwd`         |         | Working directory (`~` expanded)                  |
| `env`         |         | Environment variables                             |
| `autostart`   | `#true` | Start automatically on launch                     |
| `autorestart` | `#true` | Restart on exit                                   |
| `unfocus-key` | `Esc`   | Key to exit focus mode (for processes that need Esc)|
| `scrollback`  | global  | Override global scrollback for this process       |

Each process must have exactly one of `cmd` or `shell`.
