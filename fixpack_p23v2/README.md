# Фикс-пакет P23v2 (переиздание) — 31.10.2026

## Почему переиздан под новым именем
Пользователь установил ПЕРВУЮ сборку P23 (v1), скачав архив из карточки старого
сообщения: P23=7/6/6/6, но set_live_equity=0/0 — P16-блок затёрт. Поведения это
сегодня не меняет (в P22 main.rs нет ни одного вызова set_live_equity — проверено),
но canonical-состояние дерева = присланные файлы с P16. Переиздание v2 содержит
ровно те же 4 файла, что и первая v2-сборка: momentum_v3/keltner_dca = P23 + P16,
bounce/liquidity — без изменений (уже установлены байт-в-байт те же).

## ⚠ ОБЯЗАТЕЛЬНЫЙ пре-чек (развилка)
```powershell
Select-String -Path src\strategies\mod.rs -Pattern 'set_live_equity' | Measure-Object | % Count
```
- **>= 1** — трейт Strategy объявляет метод, v2 соберётся. Продолжай установку.
- **0** — НЕ устанавливать v2 (будет ошибка E0407). Прислать src\strategies\mod.rs
  ассистенту: он вернёт mod.rs с добавленным объявлением
  `fn set_live_equity(&mut self, _eq: f64) {}` в трейт, установка повторится.

## Установка (PowerShell, из C:\Users\BASE_PC\scalping_agentsGLM)

0) Скачать `scalping_fixpack_p23v2_20261031.zip` ИЗ ПОСЛЕДНЕГО сообщения чата
   (не из старых карточек!) в корень проекта и сверить хеш:
```powershell
Move-Item "$HOME\Downloads\scalping_fixpack_p23v2_20261031.zip" . -Force
Get-FileHash .\scalping_fixpack_p23v2_20261031.zip -Algorithm SHA256
# сверить с SHA256, опубликованным в чате вместе с пакетом
```
1) Остановить бота:
```powershell
Get-Process scalping_agents -ErrorAction SilentlyContinue | Stop-Process -Force
```
2) Распаковать и заменить 2 файла (bounce/liquidity уже корректны):
```powershell
Expand-Archive .\scalping_fixpack_p23v2_20261031.zip -DestinationPath . -Force
Copy-Item fixpack_p23v2\src\strategies\momentum_v3.rs  src\strategies\momentum_v3.rs -Force
Copy-Item fixpack_p23v2\src\strategies\keltner_dca.rs  src\strategies\keltner_dca.rs -Force
```
3) Контроль после копирования:
```powershell
Select-String -Path src\strategies\momentum_v3.rs -Pattern 'set_live_equity' | Measure-Object | % Count   # 1
Select-String -Path src\strategies\keltner_dca.rs  -Pattern 'set_live_equity' | Measure-Object | % Count   # 1
Select-String -Path src\strategies\momentum_v3.rs -Pattern 'P23' | Measure-Object | % Count               # 6
Select-String -Path src\strategies\keltner_dca.rs  -Pattern 'P23' | Measure-Object | % Count              # 6
```
Строгая проверка (хеши должны совпасть):
```powershell
Get-FileHash src\strategies\momentum_v3.rs -Algorithm SHA256   # 58b3323fd608c3b247f681ef083f676b2953781c7c734b5628b275c6252e0aca
Get-FileHash src\strategies\keltner_dca.rs -Algorithm SHA256   # c4411dab88390e362a77a4f3ad1d805650c425bc9f5656250dd4b00added506a
```
4) Сборка и запуск ОДНОГО экземпляра:
```powershell
cargo build --release
.\target\release\scalping_agents.exe
```
В стартовом логе НЕ должно быть `os error 10048`.

## Что делает P23 (без изменений с первой сборки)
1. Живой спред: стратегии ловят bookTicker (bid/ask).
2. Полная стоимость спреда за круг вычитается из net_rr (симметрично комиссии).
3. Slippage-guard: спред/мид > P23_MAX_SPREAD_PCT (по умолч. 0.0010) — вход запрещён.
4. diag: spr_block=N (у momentum — spr=N).

## Контроль в бою
- diag всех 4 ботов: spr_block=0 (momentum: spr=0); растёт при блокировке по спреду.

## Откат
```powershell
Copy-Item fixpack_p23v2\orig\momentum_v3.rs.orig  src\strategies\momentum_v3.rs -Force
Copy-Item fixpack_p23v2\orig\keltner_dca.rs.orig  src\strategies\keltner_dca.rs -Force
cargo build --release
```
orig = присланные пользователем файлы (P22-эра: P16 есть, P23 нет).
Папку fixpack_p23 (от v1) можно удалить.

## Хеши файлов пакета
- bounce_trader.rs: bea500e1ca27e377c34741bcf4197cedf32c6db43a7407cfc0e61ddfd9b1253a
- liquidity_grab.rs: 30f13fcd0d4c07f6611a39c4a31ccb2f695ca8a49521e2d2dcbbd2797d39d378
- momentum_v3.rs: 58b3323fd608c3b247f681ef083f676b2953781c7c734b5628b275c6252e0aca
- keltner_dca.rs: c4411dab88390e362a77a4f3ad1d805650c425bc9f5656250dd4b00added506a
