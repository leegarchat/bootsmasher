# bootsmasher

Статичный самодостаточный CLI для хирургии Android boot-образов. Шесть подпрограмм (короткие алиасы в скобках):

- `vboot` [`vb`] — умный ремонтный флоу `vendor_boot` (специализация Pixel 6):
  нормализация протухшей таблицы, замена платформы,
  `--split-first-stage` (`--split`) / `--merge`, фильтр фрагментов `--drop`,
  правило first-stage inbuild-vs-cpio, предэмиссионная проверка, режим stdout,
  гейт места `--min-free`.
- `unpack` [`u`|`up`] — извлечение в духе magiskboot для `boot.img` (v0..v4, ядра,
  dtb/dtbo) **и** `vendor_boot`: автодекомпрессия, вердикты по секциям,
  отчёт, который не падает (битые части всё равно дампятся, с точным ПОЧЕМУ).
- `repack` [`r`|`rp`] — пересборка из каталога распаковки: `spec.toml` или
  `--base` (`-b`), `--template` (`-t`), `--set` (`-s`), `--format` (`-f`),
  управление футером, проверка перед записью.
- `cpio` [`c`] — правка newc-архива на месте, точный порт magiskboot:
  `exists/ls/rm/mkdir/ln/mv/add/extract/test/patch/backup/restore`.
- `compress` [`cmp`] — сжать один файл именованным кодеком
  (`compress[=format]`, по умолчанию gzip): gzip/xz/lzma/lz4/lz4_legacy.
- `decompress` [`dcmp`] — распаковать один архив, формат по магии.

`bootsmasher help [подпрограмма]` печатает мануал подпрограммы; каждая
подпрограмма также отвечает на `--help`. Коды выхода везде: 0 ок, 1 ошибка
использования, 2 битый вход / провал проверки.

Только чистый Rust (`lz4_flex`, `flate2`, `lzma-rust2`, `serde`/`toml`;
биндинг `libc` statvfs только на Unix), без внешних команд и C-кода;
статика musl под Linux (x86_64/x86/aarch64/armv7) и сборки MinGW/LLVM под
Windows (x86_64/aarch64).

## unpack (`u`, `up`)

```text
bootsmasher unpack <образ> [-h] [-n] [-o <каталог>] [-x] [--no-spec]
```

Сам определяет `ANDROID!`/`VNDRBOOT`, раскладывает файлы с именами как у
magiskboot (`kernel`, `kernel_dtb`, `ramdisk.cpio`, `second`, `extra`,
`recovery_dtbo`, `dtb`, `signature`, `bootconfig`, `header`, плюс
`vendor_ramdisk/<имя>.cpio`, `footer.bin` и наш `spec.toml`):

- `-h` пишет файл `header` как у magiskboot; `spec.toml` пишется всегда
  (полная точность: форматы, размеры, board_id, футер), кроме `--no-spec`.
- По умолчанию kernel/ramdisk/extra декомпрессируются на лету (формат по
  магии: gzip/xz/lzma/lz4-frame/lz4-legacy); `-n` оставляет исходные байты.
- Фрагмент, который не декомпрессируется, всё равно дампится КАК ЕСТЬ с
  пометкой `INVALID` — прогон не абортится, как это делает magiskboot.
- `-x`, `--extract` разворачивает каждый годный cpio в `<файл>.d/` (файлы,
  каталоги, симлинки, unix-права).
- По битым образам — ПОЧЕМУ по каждой секции (`block 6 truncated: need
  X, have Y — таблица режет один поток посередине блока; сумма таблицы
  vs заголовок, diff N`), а для протухшей таблицы vendor_boot —
  `vendor_ramdisk/ramdisk.full-rescue.cpio` со всем бLOBом как одним
  валидным потоком. Финал `RESULT: OK` (exit 0) или `RESULT: DEGRADED`
  (тоже exit 0; exit 2 — только когда не читается даже заголовок).

## repack (`r`, `rp`)

```text
bootsmasher repack [каталог="."] [выход="new-boot.img"] [-b <образ>] [-t <образ>]
                   [-s k=v]... [-f цель=fmt]...
                   [-n] [--drop-footer] [--pad-to N] [--min-free S]
                   [--check-dir <каталог>]
```

- Раскладка из `каталог/spec.toml`; без него `--base` даёт размеры,
  форматы и байты недостающих файлов (паритет magiskboot: заменяют только
  присутствующие файлы). Без обоих раскладка неизвестна (exit 1).
- Скаляры заголовка: spec/base → `каталог/header` (паритет magiskboot) →
  `--template` → `--set` (`cmdline|name|os_version|os_patch_level|
  page_size|kernel_addr|ramdisk_addr|second_addr|tags_addr|dtb_addr`).
- Форматы посекционно (`--format ramdisk.cpio=gzip`, группы `ramdisk`/
  `all`), по умолчанию из spec/base/детекта; ramdisk v4-boot форсится в
  `lz4_legacy` (правило GKI-мержа, как у magiskboot); уже сжатые файлы
  копируются как есть; `-n` отключает сжатие.
- Таблица vendor пересобирается (офсеты перецепляются, имена/типы/
  board_id сохраняются); у boot освежается офсет `recovery_dtbo`;
  `kernel_dtb` учитывается при явном файле kernel.
- Футер сохраняется (`footer.bin`, иначе хвост `--base`), кроме
  `--drop-footer`. Выход перепарсивается и перепроверяется в памяти; при
  провале ничего не пишется. Гейт места: каталог выхода должен вместить
  образ + `--min-free` (байты или `512M`).

```sh
bootsmasher unpack vendor_boot.img -o dir -h -x
bootsmasher u boot.img -o dir -n
bootsmasher repack dir fixed.img
bootsmasher r dir fox.img -b stock.img -f ramdisk.cpio=gzip
bootsmasher repack pinit/ init_new.img --set cmdline="console=ttyS0" -n
```

## cpio (`c`)

```text
bootsmasher cpio <incpio> [команды...]
```

Точный порт cpio из magiskboot: правка newc-архива (`070701`) на месте.
Каждая команда — один закавыченный аргумент шелла; файл перезаписывается
после последней команды (`ls`/`test`/`exists` только сообщают и выходят
без записи; отсутствующий `<incpio>` начинает пустой архив). Вход должен
быть сырым newc — `unpack` без `-n` его уже пишет, либо распакуйте `.lz4`.

Команды: `exists ENTRY` (0/1) | `ls [-r] [PATH]` | `rm [-r] ENTRY` |
`mkdir MODE ENTRY` (восьмеричный) | `ln TARGET ENTRY` |
`mv SOURCE DEST` | `add MODE ENTRY INFILE` | `extract [ENTRY OUT]` |
`test` (0 stock / 1 Magisk / 2 unsupported) |
`patch` (чистка verify/avb/forceencrypt из fstab, `KEEPVERITY` /
`KEEPFORCEENCRYPT=true` сохраняет) | `backup ORIG [-n]` (дифф в
`.backup/`, xz кроме `-n`) | `restore` (xz-записи распаковываются назад).

```sh
bootsmasher cpio ramdisk.cpio "exists init" "ls -r /system"
bootsmasher c ramdisk.cpio "add 644 new.rc ./new.rc" "ls new.rc"
bootsmasher cpio ramdisk.cpio patch
bootsmasher cpio ramdisk.cpio test; echo $?
```

## compress / decompress (`cmp` / `dcmp`)

```text
bootsmasher compress[=format] <вход> [выход]
bootsmasher decompress <вход> [выход]
```

Паритет magiskboot для однофайловых кодеков поверх `src/codec.rs`
(gzip/xz/lzma/lz4-frame/lz4-legacy sniff + транскодинг, чистый Rust):

- `compress` по умолчанию gzip; явно `compress=xz`, `compress=lzma`,
  `compress=lz4`, `compress=lz4_legacy` (алиас `lz4_lg`). Неизвестный
  формат — как у magiskboot:
  `Unsupported or unknown compression format: ...` (exit 1).
- `decompress` определяет формат по магии, печатает
  `Detected format: <имя>` в stderr и распаковывает. Нет знакомой магии
  (или чужое расширение архива) — `Input file is not a supported type!`
  (exit 2).
- `-` с любой стороны — бинарные stdin/stdout. Без `[выхода]` входной
  файл заменяется: compress дописывает расширение формата
  (`ramdisk.cpio` → `ramdisk.cpio.gz`; у lz4_legacy общий `.lz4` с lz4,
  ровно как у magiskboot), decompress его снимает.
- Единственное сознательное отклонение: **bzip2 не встроен** (сторонние
  зависимости запрещены — только то, что уже в Cargo.toml).
  `compress=bzip2` — usage-ошибка (`bzip2 not supported in this build`),
  а `BZh`-блоб на `decompress` — ошибка `unsupported format` (exit 2).

```sh
bootsmasher compress ramdisk.cpio                  # -> ramdisk.cpio.gz
bootsmasher compress=xz ramdisk.cpio ramdisk.cpio.xz
bootsmasher decompress ramdisk.cpio.gz             # -> ramdisk.cpio
cat ramdisk.cpio | bootsmasher compress - - | bootsmasher decompress - -
```

## Зачем

Кривой мейнтейнерский `vendor_boot` содержал один LZ4-legacy поток на весь
блоб, а таблица ramdisk по-прежнему описывала два фрагмента
(`platform 22682993 + dlkm 7345789`, сумма на 5388 меньше заголовка).
Бутлоадер такой образ грузит (DTB он ищет по размеру из заголовка), а
прошивка фрагментов через `fastbootd` падает (`Old offset mismatch`), и
`magiskboot` падает с `failed to fill whole buffer`. `bootsmasher vboot`
не гадает, а проверяет: каждый фрагмент декомпрессируется и его cpio
валидируется, сумма таблицы сверяется с заголовком, FDT проходятся.

## Использование (`vb`)

```text
bootsmasher vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
bootsmasher vboot <vboot.img> [platform] -o <out.img> [--pad-to <байты>]
bootsmasher vboot --verify <vboot.img> [platform] [--check-dir <каталог>]
```

Режимы раскладки (взаимоисключающие):
- без флага — сохранить раскладку (валидный образ проходит байт-в-байт;
  протухшая таблица нормализуется в одну запись platform). С файлом
  платформы она заменяется; валидный исходный dlkm сохраняется как фолбэк,
  иначе `lib/**` выносится в свежий dlkm.
- `--split-first-stage` — разбить содержимое по поддеревьям:
  `first_stage_ramdisk/**` + остальное → platform, `recovery/**` +
  `debug_ramdisk/**` → recovery, `lib/**` → dlkm. Фрагмент dlkm/recovery
  создаётся только при наличии реальных файлов (голый каталог `lib` или
  `debug_ramdisk` фрагмента не стоит); при отсутствии своего dlkm-наполнения
  валидный исходный dlkm сохраняется байт-в-байт.
- `--merge` — склеить всё в один фрагмент platform (слоты platform
  заменяются новым файлом, если дан; исходное содержимое dlkm/recovery
  присоединяется; серединные TRAILERы выкидываются, пишется ровно один).
- `--drop <sel,...>` — удалить исходные фрагменты по типу или имени из
  таблицы (`platform`, `dlkm`, `recovery`, `none`, `16K`, ...; повторяется,
  через запятую, регистр важен), во всех режимах: Keep пропускает их
  (выжившие смыкают offsets), Split/Merge исключают их из пула, carryover
  и дословного dlkm-фолбэка. Селектор, ничего не нашедший, предупреждает
  в stderr и игнорируется. `first-stage` — особый, см. ниже.

Правило first-stage (с файлом платформы): собственный
`first_stage_ramdisk/**` базового образа (inbuild) сохраняется по
умолчанию — эти записи идут первыми, first-stage-записи переданного cpio
удаляются попутно. Добавь `first-stage` в `--drop`, чтобы инвертировать
правило (победит first-stage из cpio, inbuild удалится). Без файла
платформы `--drop first-stage` вычищает `first_stage_ramdisk/**` из пула
записей. Корневой `init` — обычный platform-пейлоад, не first-stage.

Перекодированные фрагменты — marker-free LZ4-legacy, ровно как
kernel-рамдиски (фрагменты стыкуются впритык; `lz4` CLI считает нулевое слово
порчей, поэтому end-marker не пишется).

- Без файла платформы: нормализация раскладки. Протухшая однофрагментная
  таблица становится одной записью `platform`; валидный образ проходит
  байт-в-байт (фрагменты не перекомпрессируются, а копируются как есть).
- С файлом платформы (`.lz4` после проверки остаётся как есть, сырой `.cpio`
  жмётся в LZ4-legacy): платформа заменяется. Валидный исходный фрагмент
  `dlkm` сохраняется как фолбэк, иначе `lib/**` выносится из новой платформы
  в свежий фрагмент `dlkm`. Флаги заголовка, cmdline, DTB и bootconfig
  сохраняются.
- Собранный образ полностью перепроверяется в памяти до выдачи; при ошибке
  ничего не пишется.
- Без пути вывода образ идёт в **stdout** без постороннего мусора
  (диагностика — только в stderr). `--pad-to 67108864` добивает нулями до
  размера блочного устройства.
- Перед записью проверяется свободное место: `free(каталог)` должно покрыть
  образ плюс `--min-free` (по умолчанию 0; чистые байты или human-размеры
  вроде `512M`, `1GiB`). Проверяемый каталог — родитель выходного файла
  (или `--check-dir`); для вывода в stdout проверка только с `--check-dir`.
- `vboot --verify base.img platform.cpio.lz4` — сухой прогон всего пайплайна
  в памяти: итоговая раскладка, итоговый размер и вердикт по месту
  (каталог по умолчанию `.`) — и ничего не пишет.
- Коды выхода: `0` ок, `1` ошибка использования, `2` битый вход / провал
  проверки.

```sh
bootsmasher vboot --verify vendor_boot.img
bootsmasher vboot broken_vendor_boot.img -o fixed.img
bootsmasher vboot stock_vendor_boot.img OrangeFox.ramdisk.lz4 -o fox_boot.img
bootsmasher vboot stock_vendor_boot.img --merge -o single.img
bootsmasher vboot shiba_vendor_boot.img fox.cpio --drop 16K -o fox_shiba.img
bootsmasher vboot shiba_vendor_boot.img fox.cpio --drop 16K,first-stage -o fox2.img
bootsmasher vboot laguna_vendor_boot.img --drop recovery -o plat_only.img
bootsmasher vboot broken.img full.cpio --split -o frag.img
bootsmasher vboot broken.img fox.lz4 --pad-to 67108864 --min-free 1G -o fox_64m.img
bootsmasher vboot broken.img > fixed.img

## Сборка

```sh
./build.sh --arch x64        # быстрый локальный путь
./build.sh --arch all        # 4 Linux musl + 2 Windows (gnu x64, gnullvm arm64)
./build.sh --cross --arch windows
```

Для Linux нужны musl + кросс-gcc; для Windows x86_64 — `mingw-w64` и
`rustup target add x86_64-pc-windows-gnu`. Для Windows ARM64 дистрибутивного
тулчейна нет: его Rust-таргет — `aarch64-pc-windows-gnullvm`
(`aarch64-pc-windows-gnu` не существует), линкуется через llvm-mingw
(mstorsjo), который `build.sh` подхватывает из `/opt/llvm-mingw`:

```sh
curl -LO https://github.com/mstorsjo/llvm-mingw/releases/download/20240619/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz
sudo tar -xf llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz -C /opt/
sudo mv /opt/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64 /opt/llvm-mingw
rustup target add aarch64-pc-windows-gnullvm
```

(У `cross` нет образа для `aarch64-pc-windows-gnullvm`, он откатывается на
хостовый cargo — это и есть путь выше.) Результаты падают в `dist/`:
4 статических musl ELF + 2 Windows PE, всё проверено с чистого дерева
с нулем предупреждений `rustc`.

## Тесты

```sh
cargo build --release
./test_vboot.sh              # нужны образы-фикстуры, без них скипается
```

Фикстуры (переопределяются через `$BROKEN`, `$STOCK`, `$FOX`): битый Pixel 6
`vendor_boot`, стоковый LOS `raven` `vendor_boot`,
`OrangeFox-R12.0-test_1-gs101.ramdisk.lz4`. Сьюта проверяет вердикты verify,
байтовую идентичность round-trip, verbatim-сохранение dlkm, чистоту stdout,
`--pad-to` и опциональный кросс-чек `magiskboot`.

## Структура

```text
Cargo.toml            lz4_flex + flate2 + lzma-rust2 + serde/toml (+ libc-биндинг statvfs на Unix);
                      release: LTO fat, abort, strip
build.sh              статическая мультиарх-сборка (linux musl x4 + windows gnu x2)
src/main.rs           диспетчер подпрограмм (vboot | unpack | repack | cpio | compress | decompress)
src/error.rs          типы ошибок (Usage/Fail/Io/Parse/Verify), диагностика только в stderr
src/bootimg.rs        парсинг/сборка ANDROID! v0..v4, отщепление kernel_dtb
src/codec.rs          sniff + транскодинг gzip/xz/lzma/lz4-frame/lz4-legacy
src/compress_cmd.rs   подпрограммы compress[=fmt]/decompress (паритет magiskboot, bzip2 отклонён)
src/cpiox.rs          распаковка cpio в каталог (безопасные пути, симлинки, права)
src/cpio_cmd.rs       подпрограмма cpio (порт magiskboot: 12 команд на месте)
src/spec.rs           запись раскладки spec.toml (serde/toml, hex board_id)
src/unpack.rs         подпрограмма unpack (boot + vendor, diagnose, rescue)
src/repack.rs         подпрограмма repack (spec/base/template/set/format/footer)
src/vboot/mod.rs      CLI vboot (позиционные + -o/--out/--pad-to/--verify/--split-first-stage/--merge/--drop/--min-free/--check-dir)
src/vboot/image.rs    структуры заголовка vendor_boot v3/v4 и таблицы
src/vboot/lz4legacy.rs  marker-free фрейминг LZ4-legacy поверх блочного кодека lz4_flex
src/vboot/cpio.rs     парсинг/сборка/разбиение newc (lib/** = dlkm, recovery|debug_ramdisk/** = recovery), терпим к 512-падам
src/vboot/dtb.rs      проходчик склеенных FDT
src/vboot/ops.rs      анализатор + перепаковка (Keep/Split/Merge) + предэмиссионный верифаер + диагноз для unpack
src/vboot/space.rs    парсинг размеров + проверка места перед записью (statvfs / GetDiskFreeSpaceExW)
test_vboot.sh         сьюта vboot (21 проверка, вкл. carryover 16K), хелпер test_compare_dlkm.py
test_unpack_repack.sh сьюта unpack/repack/cpio/compress (35 проверок: GKI boot, init_boot,
                      recovery vendor_boot, roundtrip-ы, -n идентичность, base
                      fallback, set/format, refuse-invalid, template, место,
                      edit-wins, алиасы, cpio patch/backup/restore,
                      compress/decompress roundtrip-ы + дефолт + pipe + плохой формат)
```

## Лицензия

MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
