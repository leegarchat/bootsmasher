//! `pick` help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} pick — arrow-key menu for installer scripts

Usage:
  {prog} pick [--prompt TEXT] [--default N] opt1 [opt2 ...]
  {prog} pick --help | --expand

What it does:
  Shows the options as an interactive list (Up/Down/j/k/Tab + Enter),
  prints the chosen option to stdout and exits 0. Esc or the q key
  aborts: stdout stays empty, stderr gets 'aborted by user', exit 1 —
  scripts treat any non-zero exit plus empty stdout as 'user walked
  away'. (Ctrl-C kills via SIGINT instead — standard terminal
  behavior, not an exit code.)

Options:
  --prompt TEXT  question line above the list (default: 'Select:')
  --default N    preselected option, 1-based (empty Enter picks it).
                 Without a terminal on stdin the default wins outright;
                 without it that is a usage error (exit 1).

Examples:
  {prog} pick --prompt 'Slot:' --default 3 'only a' 'only b' 'both (a+b)'
  {prog} pick --prompt 'Flash?' 'Yes, flash' 'Exit'

Exit codes: 0 chosen, 1 usage error or aborted by user,
2 broken input / I/O failure.
Details: {prog} pick --expand"
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} pick — details (see `{prog} pick --help` for the short form)

  The installer (install.sh) drives every user question through pick:
  device serials, slot choice, Continue/Exit pacing, the flash
  confirmation. One implementation serves both shells: install.sh and
  install.ps1 call the same binary, so the menus behave identically
  on Linux and Windows.

  Notes:
  - Option values starting with '-' are flags, not options (serials,
    slot labels and menu texts never do).
  - An option literally named '--help' would print this text instead;
    menu labels must avoid it.
  - Piped stdin (CI, --force flows) never blocks: --default answers,
    otherwise exit 1. The menu itself always needs a real terminal.
  - Without --default the first Down/j/Tab only activates the top
    entry (Up/k jump to the bottom); the installer always passes
    --default, so Enter answers immediately."
    )
}
