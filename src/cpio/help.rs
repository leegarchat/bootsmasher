//! `cpio` help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} cpio — in-place newc archive surgery (magiskboot port)

Usage:
  {prog} cpio <incpio> [commands...]
  {prog} cpio --help | --expand

Each command is one shell-quoted argument; the file is rewritten after
the last command (ls/test/exists only report). A missing <incpio>
starts an empty archive. Input must be raw newc (070701).

Commands:
  exists ENTRY | ls [-r] [PATH] | rm [-r] ENTRY | mkdir MODE ENTRY |
  ln TARGET ENTRY | mv SOURCE DEST | add MODE ENTRY INFILE |
  extract [ENTRY OUT] | test | patch | backup ORIG [-n] | restore

Examples:
  {prog} cpio ramdisk.cpio \"exists init\" \"ls -r /system\"
  {prog} cpio ramdisk.cpio \"add 644 new.rc ./new.rc\" \"ls new.rc\"
  {prog} cpio ramdisk.cpio patch

Exit codes: 0 ok (test: 0 stock; exists: 0 found), 1 usage error
(also exists-missing, test Magisk/unsupported), 2 broken input.
Details: {prog} cpio --expand"
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} cpio — details (see `{prog} cpio --help` for the short form)

Command semantics (magiskboot parity):
  exists ENTRY: exit 0 if present, else 1; nothing written.
  ls [-r] [PATH]: list PATH (default /); -r recurses. Prints
    '<mode> <uid> <gid> <size> <rdev>  <name>' per entry to stdout.
  rm [-r] ENTRY: remove one entry; -r removes the whole subtree.
  mkdir MODE ENTRY: create dir with octal MODE (e.g. 755).
  ln TARGET ENTRY: symlink ENTRY -> TARGET.
  mv SOURCE DEST: rename; error if SOURCE is missing.
  add MODE ENTRY INFILE: store host file INFILE as ENTRY (replaces).
    Symlinks are stored as regular files (use ln for links);
    block/char devices keep device numbers. ENTRY must not end
    with '/'.
  extract [ENTRY OUT]: extract ENTRY to OUT (parents created, unix
    modes applied); no args extracts everything into the current dir.
  test: exit code only — 0 stock, 1 Magisk-patched, 2 unsupported
    (SuperSU/xposed).
  patch: strip verify/avb/forceencrypt flags from fstab files (honors
    KEEPVERITY / KEEPFORCEENCRYPT env, \"true\" keeps).
  backup ORIG [-n]: diff ORIG against <incpio>; changed/removed ORIG
    entries land in .backup/ (xz unless -n), new entries in
    .backup/.rmlist.
  restore: restore from the embedded .backup (xz entries decoded).

Notes:
  Inodes renumber from 300000 on dump; concatenated archives (several
  070701 streams back to back) load as one. Unix-only bits (mknod,
  device nodes, modes) are cfg-gated so Windows targets still build."
    )
}
