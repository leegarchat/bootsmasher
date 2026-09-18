# bootsmasher

Статичный самодостаточный CLI для хирургии Android boot-образов. Подпрограмма
`vboot` (v0.1.0) анализирует и перепаковывает `vendor_boot` v3/v4, специализация
— Pixel 6 (gs101, page size 2048). Только чистый Rust (`lz4_flex`), без внешних
команд и C-зависимостей; статика musl под Linux (x86_64/x86/aarch64/armv7) и
сборки MinGW под Windows (x86_64/aarch64).

## Зачем

Кривой мейнтейнерский `vendor_boot` содержал один LZ4-legacy поток на весь
блоб, а таблица ramdisk по-прежнему описывала два фрагмента
(`platform 22682993 + dlkm 7345789`, сумма на 5388 меньше заголовка).
Бутлоадер такой образ грузит (DTB он ищет по размеру из заголовка), а
прошивка фрагментов через `fastbootd` падает (`Old offset mismatch`), и
`magiskboot` падает с `failed to fill whole buffer`. `bootsmasher vboot`
не гадает, а проверяет: каждый фрагмент декомпрессируется и его cpio
валидируется, сумма таблицы сверяется с заголовком, FDT проходятся.

## Использование

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
bootsmasher vboot broken.img full.cpio --split-first-stage -o frag.img
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
Cargo.toml            только lz4_flex (+ libc-биндинг statvfs на Unix); release: LTO fat, abort, strip
build.sh              статическая мультиарх-сборка (linux musl x4 + windows gnu x2)
src/main.rs           диспетчер подпрограмм (в 0.1.0 только vboot)
src/error.rs          один тип ошибок, диагностика только в stderr
src/vboot/mod.rs      CLI vboot (позиционные + -o/--out/--pad-to/--verify/--split-first-stage/--merge/--min-free/--check-dir)
src/vboot/image.rs    структуры заголовка vendor_boot v3/v4 и таблицы
src/vboot/lz4legacy.rs  marker-free фрейминг LZ4-legacy поверх блочного кодека lz4_flex
src/vboot/cpio.rs     парсинг/сборка/разбиение newc (lib/** = dlkm, recovery|debug_ramdisk/** = recovery), терпим к 512-падам
src/vboot/dtb.rs      проходчик склеенных FDT
src/vboot/ops.rs      анализатор + перепаковка (Keep/Split/Merge) + предэмиссионный верифаер
src/vboot/space.rs    парсинг размеров + проверка места перед записью (statvfs / GetDiskFreeSpaceExW)
test_vboot.sh         функциональные тесты (20 проверок), хелпер test_compare_dlkm.py
```

## Лицензия

MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
