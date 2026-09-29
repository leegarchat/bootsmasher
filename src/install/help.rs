//! `bootsmasher install` help texts. `{prog}` is the argv[0] basename.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} install — OrangeFox vendor_boot installer (Pixel shiba and friends).

Everything runs in the BOOTLOADER (classic fastboot, NOT fastbootd):
fetch, flash and getvar all work there, so the device never boots
system/fastbootd during install.

Flow:
  [1/5] device: fastboot, system adb and recovery adb are all picked
        up; several endpoints get one menu where every entry shows
        its mode (so adb+adb or adb+bootloader pairs stay
        unambiguous). An adb device is root-probed (direct shell,
        then su): with root you choose the installer branch —
        bootloader/fastboot (recommended) or rooted adb (no
        reboot); without root the only path is the bootloader, and
        every bootloader choice is followed by a reboot-readiness
        question
  [2/5] Install OrangeFox | Restore backup (+ slot/backup picks;
        restore also asks \"fresh backup first?\" and the backup folder
        name — stamp by default, or a custom one; \"Back\" steps one
        level up, \"Exit\"/Esc/q aborts cleanly)
  [3/5] backup current stock -> backup/<name>/ (skipped only when the
        restore flow is told to; install always backs up, safety first)
  [4/5] rebuild with fox (install) or stage backup images (restore;
        restore flashes them byte-identical, no checks, no repack);
        short per-slot report (install only); flash confirmation
  [5/5] flash + fetch-back byte check; on success the installer asks
        whether to reboot the device to recovery (default: stay in
        the bootloader; never asked under --force/--file)

Console stays short; every tool's full output goes to
logs/<datetime>/install.log (logs live apart from image backups).
All questions are arrow-key menus
(dialoguer, same code on Linux and Windows).

Paths (fastboot/adb, recovery payload, backup dir) come from export.txt:
KEY=VALUE lines, # comments; relative paths resolve against the
directory holding export.txt. Lookup: --export FILE, then
export.txt next to the binary, then ./export.txt.
--recovery-img PATH overrides export.txt RECOVERY_IMG for this run
(drag-and-drop in the desktop launchers forwards the dropped cpio
here, so export.txt never needs editing; a relative PATH resolves
against the working directory).

Usage:
  {prog} install [--force] [--slot a|b|both] [--mode install|restore]
                 [--backup latest|STAMP] [--transport fastboot|adb]
                 [--export FILE] [--recovery-img PATH]
  {prog} install --demo   (UI preview: canned device, same menus and
                 screens, no fastboot/adb, nothing read or flashed)
  {prog} install --file -i INPUT -c CPIOPAYLOAD -o OUTPUT [--log FILE]
                 [--recovery-is-platform var1|var2]
                 (recovery install into a plain image file: verify input,
                 rebuild with the payload, verify output, write; no
                 device, no backup, no menus; built for recovery use)
  install.sh / install.bat are thin launchers forwarding to this.

Install layout is fixed to Type A (classic recovery fragment) for
now — the layout question is skipped; --recovery-is-platform and
--file still select B/C under the hood.

Exit codes: 0 ok (or clean user abort, nothing flashed),
1 usage / no device, 2 build/verify/flash failure."
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} install — OrangeFox vendor_boot installer, in detail.

export.txt keys (all optional, built-ins shown):
  RECOVERY_IMG=OrangeFox-R12.0-test5-aio.ramdisk.lz4
  BACKUP_DIR=backup        (stock image backups: backup/<name>/)
  LOG_DIR=logs             (per-run logs: logs/<datetime>/install.log;
                           the desktop launchers find the run log there
                           for the reboot-to-recovery offer)
  PLATFORM_TOOLS_LINUX=platform-tools-linux
  PLATFORM_TOOLS_WINDOWS=platform-tools-windows
  FASTBOOT_BIN=fastboot        (resolved inside the platform-tools dir;
  ADB_BIN=adb                   .exe is appended on Windows when missing)
  FALLBACK_PATH=0              (opt-in PATH fallback: 1/true/yes/on lets
                               a missing or wrong-CPU bundled binary fall
                               back to fastboot/adb from PATH, each tool
                               independently; off by default)

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
  mode    Install OrangeFox | Restore backup (asked only when
          backups already exist)
  slot    Slot a only | Slot b only | Both (a+b)
  backup  newest first, labelled \"latest: <stamp> (<slots>)\"
          (restore only)
  fresh   Fresh backup first? yes (default) | no, flash without a
          safety copy (restore only; install always backs up,
          --force restore assumes yes into the run stamp)
  folder  Backup folder: the run stamp, or a custom single-segment
          name typed in (restore with fresh backup only; an existing
          non-empty folder is never reused silently)
  pacing  Ready to proceed (gate before the fetch)

Restore flashes the picked backup images byte-identical: no rebuild,
no repack, no validity or free-space gates — a missing image file is
the only refusal. A fetch-back byte comparison after flashing still
proves what landed on the device.

Install layout is fixed to Type A for now (the menu is temporarily
disabled; install always goes the classic route). --recovery-is-platform
var1|var2 and --file still select Type B/C under the hood; setting
BOOTSMASHER_LAYOUT_MENU=1 brings the menu back (dev escape hatch).

Non-interactive: --force assumes install + both slots (mode/slot
flags still narrow it); without a terminal on stdin --force is
required. --transport pins the installer branch: fastboot (classic
bootloader rails, --force reboots there silently) or adb (rooted-adb
path, fails when the device refuses the block read). Without the
flag --force keeps the legacy behavior (straight to the bootloader).

Adb-root transport (system or recovery adb with block access,
directly or via su): stock is read with `dd if=/dev/block/by-name/
vendor_boot_X of=/data/local/ofox_installer/...` + `pull`, patched
locally exactly like the fastboot flow, then `push`ed back and
written with `dd` after a best-effort `blockdev --setrw`; restore
pushes the stored backup the same way. No fastboot traffic at all;
the final reboot-to-recovery offer goes out over adb.

File mode (--file) needs no export.txt, no device and no terminal:
the input image is verified, rebuilt with the cpio payload (vbmeta
footer dropped, same recovery-install layout as the device flow),
the result is verified again and only then written. Input and output
must not be the same file. Short status lines go to stdout, verdict
details to stderr; --log FILE additionally tees both into the file.

Install layouts (questionnaire Type A/B/C, --recovery-is-platform):
  Type A (default): classic — payload becomes the recovery fragment,
  native first-stage wins (stock behavior, all families).
  Type B (var1): all-in-platform — native first_stage + payload merged
  into the platform fragment, no recovery fragment; test layout for
  merged-platform stocks (gs101).
  Type C (var2): payload-only platform — native platform fully replaced
  by the payload (needs a var2-AIO payload built with first_stage
  inside); modules split to dlkm when present; no recovery fragment.

Exit codes: 0 ok (or clean user abort, nothing flashed),
1 usage / no device, 2 build/verify/flash failure."
    )
}
