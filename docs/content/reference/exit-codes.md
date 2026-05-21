+++
title = "Exit Codes"
weight = 3
insert_anchor_links = "heading"
+++

`stax` is built to be scripted, so its exit status is meaningful. This page
states what each code means.

## General rule

| code | meaning                                                          |
|------|------------------------------------------------------------------|
| `0`  | the command succeeded                                            |
| `1`  | the command failed — the reason is printed to stderr as `error: …` |

Any subcommand that cannot do its job exits `1`. The most common causes:

- `stax-server` is not running (every subcommand except `record` and `setup`
  needs it);
- a required argument is missing or malformed — figue prints a diagnostic
  and exits `1`;
- the daemon rejected the request — e.g. `another run is already active`,
  or `stop` / `status` with no active run.

`--help` and `--version` are not failures: they print and exit `0`.

## `stax wait`

`wait` is the subcommand whose exit code you will actually branch on, so it
is worth stating precisely:

| code | situation                                                       |
|------|-----------------------------------------------------------------|
| `0`  | the condition was met, **or** the active run reached `Stopped` cleanly |
| `1`  | the wait timed out, there was no active run, or any other error |

A timeout prints `timed out` and exits `1`. So both of these behave as you
would expect in a script:

```bash
# bail out if the run never produces enough samples
stax wait --for-samples 10000 --timeout-ms 60000 || {
  echo "gave up waiting"; exit 1
}

# block until the run finishes, then carry on
stax record -- ./bench &
stax wait && stax top -n 20
```

## `stax stop`

`stax stop` exits `1` if there is **no active run** to stop — there was
nothing to do. It exits `0` when it successfully stopped a run.

## Scripting pattern

Because failures are exit `1` with a human-readable `error:` line on stderr,
the idiomatic check is a plain `||`:

```bash
stax status || { echo "is stax-server up?"; exit 1; }
```

For the conditions `wait` understands, see
[`stax wait`](@/reference/cli.md#stax-wait); for what the errors mean, see
[Troubleshooting](@/guide/troubleshooting.md).
