//! `bootsmasher install` help texts. `{prog}` is the argv[0] basename.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} install — OrangeFox vendor_boot installer (Pixel shiba and friends).

Everything runs in the BOOTLOADER (classic fastboot, NOT fastbootd):
fetch, flash and getvar all work there, so the device never boots
system/fastbootd during install.

Flow:
  [1/5] device (auto, or arrow-key pick when several are attached)
  [2/5] install OrangeFox | restore a backup (+ slot/backup picks;
        \"Back\" steps one level up, \"Exit\"/Esc/q aborts cleanly)
  [3/5] fetch current stock -> backup/<datetime>/ (always, safety first)
  [4/5] rebuild with fox (install) or stage+verify backup (restore);
        short per-slot report; flash confirmation
  [5/5] flash + fetch-back byte check

Console stays short; every tool's full output goes to
backup/<datetime>/install.log. All questions are arrow-key menus
(dialoguer, same code on Linux and Windows).

Paths (fastboot/adb, recovery payload, backup dir) come from export.txt:
KEY=VALUE lines, # comments; relative paths resolve against the
directory holding export.txt. Lookup: --export FILE, then
export.txt next to the binary, then ./export.txt.

Usage:
  {prog} install [--force] [--slot a|b|both] [--mode install|restore]
                 [--backup latest|STAMP] [--export FILE]
  {prog} install --file -i INPUT -c CPIOPAYLOAD -o OUTPUT [--log FILE]
                 (recovery install into a plain image file: verify input,
                 rebuild with the payload, verify output, write; no
                 device, no backup, no menus; built for recovery use)
  install.sh / install.bat are thin launchers forwarding to this.

Exit codes: 0 ok (or clean user abort, nothing flashed),
1 usage / no device, 2 build/verify/flash failure."
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} install — OrangeFox vendor_boot installer, in detail.

export.txt keys (all optional, built-ins shown):
  RECOVERY_IMG=OrangeFox-R12.0-test5-aio.ramdisk.lz4
  BACKUP_DIR=backup
  PLATFORM_TOOLS_LINUX=platform-tools-linux
  PLATFORM_TOOLS_WINDOWS=platform-tools-windows
  FASTBOOT_BIN=fastboot        (resolved inside the platform-tools dir;
  ADB_BIN=adb                   .exe is appended on Windows when missing)

Free-space policy: a flashed image must leave >= MIN_FREE_MB MiB
free in the partition (export.txt, default 7). Below that (but still
fitting) the run fails: the user is asked to send install.log to
@OFRPforTensorDiscussion (https://t.me/OFRPforTensorDiscussion) and
is shown the exact --force command that waives the policy (--force
never waives the hard fit check, and restore still needs --backup
in --force mode). install-recovery.sh enforces the same policy
strictly (no bypass).

Selection stages (\"Back\" steps one level up; flag-fixed stages are
skipped AND skipped over when stepping back; the flash prompt's
\"Back\" returns to the last selection and re-runs fetch/prepare,
which are idempotent):
  mode    Install OrangeFox | Restore a backup (asked only when
          backups already exist)
  slot    only a | only b | both (a+b)
  backup  newest first, labelled \"latest: <stamp> (<slots>)\"
  pacing  Continue? (gate before the fetch)

Non-interactive: --force assumes install + both slots (mode/slot
flags still narrow it); without a terminal on stdin --force is
required.

File mode (--file) needs no export.txt, no device and no terminal:
the input image is verified, rebuilt with the cpio payload (vbmeta
footer dropped, same recovery-install layout as the device flow),
the result is verified again and only then written. Input and output
must not be the same file. Short status lines go to stdout, verdict
details to stderr; --log FILE additionally tees both into the file.

Exit codes: 0 ok (or clean user abort, nothing flashed),
1 usage / no device, 2 build/verify/flash failure."
    )
}
