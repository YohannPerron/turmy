# turmy

A TUI for [Slurm](https://slurm.schedmd.com/), which provides a convenient way to manage your cluster jobs.

> [!NOTE]
> [`turmy`](https://github.com/YohannPerron/turmy) extends the original
> [turm](https://github.com/karimknaebel/turm). The published upstream packages
> do not currently include the branch-specific changes listed below.

<img alt="turmy demo" src="https://github.com/user-attachments/assets/7daade50-def3-4bf8-bf12-df311438094e" width="100%" />

`turmy` accepts the same options as `squeue` (see [man squeue](https://slurm.schedmd.com/squeue.html#SECTION_OPTIONS)). Use `turmy --help` to get a list of all available options. For example, to show only your own jobs, sorted by descending job ID, including every state currently returned by `squeue`:
```shell
turmy --me --sort=-id --states=ALL
```

## Changes from original turm

This version is based on upstream commit
[`4e648db`](https://github.com/karimknaebel/turm/commit/4e648db) and adds:

- **Pane navigation and scrolling:** focus the job list, details, or output with
  `Tab`/`Shift-Tab`; scroll horizontally with the keyboard or mouse; and handle
  Unicode display widths when clipping and wrapping text.
- **Output selection and copying:** select output with the mouse, copy the
  selection or the complete output, and send it through OSC 52 for remote/SSH
  terminal workflows.
- **Terminal-aware live logs:** read only newly appended bytes, preserve split
  UTF-8 sequences, replace invalid UTF-8, normalize carriage returns, tabs, and
  backspaces, and strip common ANSI/OSC control sequences. Python `tqdm` progress
  bars therefore update in place instead of appearing only after completion.
- **Remembered finished jobs:** retain jobs that disappear from a successful
  `squeue` refresh, toggle them with `f`, and keep their output paths available
  for the rest of the current session.
- **Pending job-array fusion:** show pending array elements as compact Slurm
  ranges such as `35804_[80-500]`, while running elements remain individual.
- **Discoverability:** provide a compact footer, a complete in-app help dialog
  opened with `?`, and expanded control documentation.
- **Development support:** expand the deterministic mock Slurm environment with
  larger fixtures, live output helpers, and a default `uv`-managed `tqdm`
  workload.
- **Maintenance:** update Rust dependencies and CI, and add regression coverage
  for viewports, copying, terminal decoding, and the local Slurm simulator.

## Installation

Download the latest precompiled x86-64 Linux binary to `~/.local/bin`:

```shell
mkdir -p ~/.local/bin
wget https://github.com/YohannPerron/turmy/releases/latest/download/turmy-x86_64-unknown-linux-musl.tar.gz -O - | tar -xz -C ~/.local/bin/
```

Other Linux architectures are available on the
[`turmy` release page](https://github.com/YohannPerron/turmy/releases).

To check for a newer GitHub release and optionally replace the currently
installed binary, run:

```shell
turmy --update
```

The update runs entirely in the terminal, asks before installing, and verifies
the downloaded release archive against its published SHA-256 checksum before
extracting or replacing the executable.

### Shell Completion (optional)

#### Bash

In your `.bashrc`, add the following line:
```bash
eval "$(turmy completion bash)"
```

#### Zsh

In your `.zshrc`, add the following line:
```zsh
eval "$(turmy completion zsh)"
```

#### Fish

In your `config.fish` or in a separate `completions/turmy.fish` file, add the following line:
```fish
turmy completion fish | source
```

## Controls

Press `?` in `turmy` to open the complete keyboard and mouse help. The focused pane
has a green border. Use `Tab` and `Shift-Tab` to move focus between the job list,
job details, and job output panes.

| Action | Keyboard | Mouse |
| --- | --- | --- |
| Select or vertically scroll | `Up`/`Down` or `j`/`k` | Wheel over a pane |
| Scroll by half a page | `Ctrl-u` / `Ctrl-d` | |
| Scroll by a page | `PageUp` / `PageDown` | |
| Go to the beginning or end | `Home`/`End` or `g`/`G` | |
| Scroll horizontally | `Left`/`Right` or `h`/`l` | `Shift`+wheel |
| Select output text | | Drag in the output pane |
| Copy selected output | `y` or `Ctrl-c` | |
| Copy all output | `Y` | |
| Toggle stdout/stderr | `o` | |
| Toggle output wrapping | `w` | |
| Show or hide remembered finished jobs | `f` | |
| Cancel, signal, or set a time limit | `c`, `C`, or `t` | |
| Quit | `q` | |

Horizontal output scrolling is disabled while wrapping is enabled. `Ctrl` or `Alt`
with an arrow key scrolls ten lines or columns at a time; `Shift` is reserved for
horizontal mouse-wheel scrolling.

When a previously visible job disappears from a successful `squeue` refresh,
`turmy` remembers it as finished for the rest of the current session. Finished jobs
are hidden by default; press `f` to show them and access their retained log paths.
Because `squeue` no longer reports the job, `turmy` uses the neutral `FINISHED`
label rather than claiming that it completed successfully or failed. Remembered
jobs are not persisted across program restarts, and job-control actions are
disabled for them.

### Clipboard support

Copying uses the OSC 52 terminal escape sequence so that it can work across SSH.
The terminal emulator must support OSC 52 and permit clipboard writes. Terminal
multiplexers such as tmux or screen, nested SSH sessions, and terminal security
settings may block or limit it. A “Copied” status means `turmy` successfully wrote
the copy request to the terminal; OSC 52 does not provide confirmation that the
desktop clipboard accepted it.

## How it works

`turmy` obtains information about jobs by parsing the output of `squeue`.
The reason for this is that `squeue` is available on all Slurm clusters, and running it periodically is not too expensive for the Slurm controller ( particularly when [filtering by user](https://slurm.schedmd.com/squeue.html#OPT_user)).
In contrast, Slurm's C API is unstable, and Slurm's REST API is not always available and can be costly for the Slurm controller.
Another advantage is that we get free support for the exact same CLI flags as `squeue`, which users are already familiar with, for filtering and sorting the jobs.
After each successful refresh, jobs missing from the new snapshot are retained in
memory as finished jobs. Failed `squeue` commands are ignored so that a temporary
controller or command error does not mark every visible job as finished.

### Resource usage

TL;DR: `turmy` ≈ `watch -n2 squeue` + `tail -f slurm-log.out`

Special care has been taken to ensure that `turmy` is as lightweight as possible in terms of its impact on the Slurm controller and its file I/O operations.
The job queue is updated every two seconds by running `squeue`.
When there are many jobs in the queue, it is advisable to specify a single user to reduce the load on the Slurm controller (see [squeue --user](https://slurm.schedmd.com/squeue.html#OPT_user)).
`turmy` updates the currently displayed log file on every filesystem modification
notification (inotify on Linux), and it only reads newly appended bytes after the
initial read. Terminal-style control sequences are normalized incrementally before
the output is rendered.
However, since filesystem notifications are not supported for remote file systems,
such as NFS, `turmy` also polls the file for newly appended bytes every two seconds.

## Development without Slurm

For local UI testing, this repository includes a deterministic Slurm simulator with
mock implementations of `squeue`, `scancel`, and `scontrol`. It provides enough jobs
and output to exercise vertical and horizontal scrolling without a Slurm install:

```shell
./scripts/mock-slurm/run --me
```

The simulator uses `uv` to start a live Python `tqdm` progress bar for job 1001
by default. Select that job and press `o` to watch its stderr output. Set
`TURMY_MOCK_TQDM=0` when running the simulator to disable the progress task.

The simulated queue is static. To test live output updates, append one line to a job:

```shell
./scripts/mock-slurm/append-output 1001 "A new simulated training message"
```

To replay the Python `tqdm` progress bar, run this in another terminal:

```shell
./scripts/mock-slurm/tqdm-progress 1001
```

To restore the original fixture output:

```shell
./scripts/mock-slurm/reset
```

The mock commands read and write `scripts/mock-slurm/logs`. Calls to `scancel` and
`scontrol` are recorded there, so cancel, signal, and time-limit actions can be
tested without affecting a real cluster.

## Original turm star history

[![Star History Chart](https://api.star-history.com/svg?repos=karimknaebel/turm&type=Date)](https://www.star-history.com/#karimknaebel/turm&Date)
