# Noční report: orchestrator M6 → M8

M6, M7 i M8 jsou hotové a zelené v CI (GitHub Actions, KVM, root). Po každém milníku proběhlo review na opusu. Jeho kritické a důležité nálezy jsou opravené, drobnosti jsou v `docs/BACKLOG.md`.

Poslední zelený běh s kódem je **37718282273**. Commit s tímto reportem spouští ještě jeden běh, jeho výsledek je na konci reportu.

## Co je hotové

### M6: resume ze snapshotu přes userfaultfd

- Firecracker běží v `unshare -m` se stabilními cestami `/fc-vm/rootfs.ext4` a `/fc-vm/kernel/vmlinux`. Díky tomu snapshot funguje na libovolném `/dev/nbdN`. Tap má MAC `02:FC:00:00:00:06`.
- `memory/`:
  - `region.rs`: regiony z handshaku. Firecracker 1.14 posílá `page_size` i `page_size_kib`.
  - `uffd.rs`: bezpečný typ nad uffd fd (copy, wake, write-protect).
  - `handshake.rs`: přenos fd přes `SCM_RIGHTS`.
  - `server.rs`: page-fault server, faulty běží nezávisle přes `FuturesUnordered`.
  - `snapshot.rs`: export paměti přes `process_vm_readv`.
- `Sandbox::snapshot` a `SandboxBuilder::resume`. `Build` drží headery memfile a rootfs (V3).
- Chunker: stahování jednoho chunku sdílí všichni čekající, I/O běží přes `spawn_blocking`.
- `/init` envd: pokusy omezené deadlinem, závod s pádem Firecrackeru, MMDS se zapisuje před startem VM.
- **Důkaz v CI:**
  - `resume_contract`: soubor zapsaný před snapshotem je po resume vidět. Po `sync` v hostu je i v rootfs diffu, ověřeno studeným startem jen z disku.
  - Oficiální SDK testy (105 prošlo, 9 přeskočeno) projdou proti studenému i obnovenému sandboxu.

### M7: pause s diffy

- Read faulty se vkládají s `UFFDIO_COPY_MODE_WP`, tedy async write-protect. U obnoveného sandboxu se dirty stránky berou z `GET /memory/dirty`. Když Firecracker write-protect nepodporuje, použije se fallback resident − empty.
- Diff paměti i rootfs se slučuje nad rodičovskými headery. `Sandbox::pause` = snapshot + stop.
- **Důkaz v CI:** studený start, soubor A, pause, resume, přepis stránky v tmpfs a soubor B, pause, resume. Pak jsou vidět A, B i obsah tmpfs a proces `sleep` na pozadí pořád žije. Disk druhé generace je ověřený studeným startem jen z rootfs.

### M8: gRPC `SandboxService`

- Upstream `orchestrator.proto` beze změn: nemá `package`, cesty jsou `/SandboxService/*`. Codegen přes tonic 0.14.
- **RPC:** Create (resume z buildu), Update (`end_time`), List, Delete a Pause. Checkpoint vrací `Unimplemented`.
- **Lifecycle:** každý sandbox má vlastní task, který čeká na příkaz, na pád (Firecracker, NBD server nebo memory server) nebo volitelně na expiraci.
- **Registry** má stavy Starting, Running a Stopping. Id se uvolní až po dokončení úklidu, takže resume se stejným id funguje.
- **Odolnost:** Create a Pause doběhnou, i když klient spojení zruší. Binárka `orchestrator` reaguje na SIGTERM tak, že smaže všechny sandboxy. Při startu uklidí zbytky po předchozím běhu.
- Z backlogu: `Drop` pro `NbdDevice`, `setsid` pro Firecracker, cgroup controllers `+cpu +memory` a sledování smrti NBD a uffd serveru.
- **Důkaz v CI:**
  - `grpc_contract::sandbox_lifecycle_over_grpc`: Create, List, duplicitní Create vrátí `AlreadyExists`, Update, Pause, Create stejného id z pauznutého buildu přečte soubor, souběžný Create, Delete, druhý Delete vrátí `NotFound`, chybějící build vrátí `FailedPrecondition`. Nakonec nezůstane netns, cgroup, nbd ani pracovní adresář.
  - `an_expired_sandbox_is_removed`.

### Opravy, které se objevily cestou

- **Skutečná příčina flaky NBD testů:** kernel publikuje `pid` dřív, než nastaví kapacitu zařízení. Připojení teď čeká na `pid` i správnou velikost a indexy zařízení se rotují.
- **CI:**
  - SDK checkout je mimo `target`, protože rust-cache obnovoval rozbitý klon.
  - Runner se uklidí od dotnet, android, ghc a CodeQL, aby nedošel disk.
  - RustFS je připíchnutý na 1.0.1 a vytvoření bucketu se zkouší opakovaně.

## Čísla (CI runner, 2 vCPU guest, 1 GiB RAM)

| Co | Hodnota |
|---|---|
| Resume (load snapshotu, uffd, envd `/init`) | 1,5–2,1 s; poslední běh 1507–1623 ms |
| Pause obnoveného sandboxu | 556–705 ms; poslední běh 583 ms |
| Diff paměti, 1. pause (studený start, resident − empty) | ~133 MB z 1 GiB |
| Diff paměti, 2. pause (write-protect tracking) | 30–35 MB z 1 GiB |
| Diff rootfs, 1. a 2. pause | ~2,1 MB a ~200 KB |

Resume zahrnuje vytvoření síťového slotu (`ip` a `iptables`) a připojení NBD. Předem vytvořené sloty (upstream jich drží 32) jsou v backlogu a čas by zkrátily.

## Rozhodnutí, která jsem udělal za tebe (Ruling)

### M6

- Test se sdíleným fetchem stojí na FIFO, protože lokální chyba nastane okamžitě a čekající by se jinak nepotkali.
- SDK se klonuje do `$RUNNER_TEMP`, protože rust-cache obnovoval rozbitý `target/e2b-sdk`.
- Test zrušeného bootu startuje rootfs s `init=/bin/sleep -- 3600`, protože prázdný disk teď selže hned.
- `Build` je v `storage/build.rs`, aby šel testovat na macOS.
- Rodič memfile studeného sandboxu je nil header: generation 1, `base_build_id` nil.
- Úkoly 5–8 šly do CI najednou: linuxový kód lokálně nepřeložím a ušetří se tři běhy CI.
- Resume nastavuje `PUT /logger` a při chybě vypisuje diagnostiku. Bez toho nešel najít zaseklý load.
- `Region` přijímá obě pole s velikostí stránky najednou.
- NBD kontrola v `sandbox_contract` hledá jen zařízení vlastní velikosti, protože paralelní testy drží jiná zařízení.
- V testu `serves_faults` byl race: počítadlo faultů se zvyšuje až po `UFFDIO_COPY`. Test teď na počítadlo počká.
- **Cena:** u několika oprav jsem neviděl test nejdřív padat. Část linuxových testů šla do CI rovnou s kódem.

### M7

- Linuxové testy šly do CI spolu s kódem. Lokálně jsem viděl padat jen `memory_dirty`, který běží i na Macu.
- Proces na pozadí se ověřuje přes `nohup sleep`, PID v `/dev/shm` a `kill -0`, ne přes `process.List` v envd.
- `Snapshot` dostal pole `memory_bytes` a `tracked_writes`, aby test dokázal, že se použil write-protect.
- `pause` vrátí uložený snapshot i v případě, že selže úklid. Chybu úklidu zaloguje.
- Testovací soubory v tmpfs jsou v `/dev/shm/e2b`. V `/dev/shm` brání přepisu `protected_regular` a Go envd se chová stejně.
- NBD: připojení čeká na pid i velikost a indexy se rotují.
- RustFS je připíchnutý na 1.0.1.

### M8

- **Lokální expirace je opt-in** (`E2B_LOCAL_EXPIRY=1`). Výchozí chování je jako upstream: expiraci a auto-pause řídí API. Původně jsem expiraci zapnul natvrdo, ale review ukázalo, že by zabíjela auto-pause sandboxy dřív, než je API pozastaví.
- **`SlotPool` přiděluje jen indexy.** Síť se vytváří a ruší pro každý sandbox zvlášť, předem vytvořené sloty jsou v backlogu.
- `RunningSandbox.config` je v upstream proto deprecated a nevyplňuji ho, protože `#[allow]` je zakázané. Stejná data jsou v plochých polích, která API čte.
- CI maže předinstalované toolchainy kvůli místu na disku.
- Opravy z review M8 (zrušení RPC, SIGTERM) nemají deterministický padající test, stavy registry jsou pokryté unit testy.

## Co zůstalo otevřené a proč

- **M3b ublk:** potřebuje bare-metal node s kernelem 6.12+. Runner GitHubu nemá `ublk_drv`.
- **Template build, API, client-proxy a nasazení na K8s:** jsou to samostatné specy mimo tenhle cíl.
- **Backlog** (`docs/BACKLOG.md`), hlavní položky:
  - předem vytvořené síťové sloty;
  - `InfoService` a health, které API potřebuje k objevení uzlu;
  - asynchronní upload při Pause;
  - huge pages;
  - prefetch z trace;
  - memfd a `use_sync_wp`;
  - admission limity;
  - přeřazení položek s cílem „M8“.
- Startovní úklid v binárce předpokládá dedikovaný node: maže všechny `ns-<N>`, `/sys/fs/cgroup/e2b/*` a `E2B_WORK_DIR`.
- Na tvém Macu nic nespouštěj. Všechno linuxové běží v CI.

## Kde to je

- Větev `main` v `Empatixx/e2b-runtime-rs`, všechno pushnuté.
- **Plány:** `docs/superpowers/plans/2026-10-10-orchestrator-m6-resume.md`, `2026-10-11-orchestrator-m7-pause.md` a `2026-10-12-orchestrator-m8-grpc.md`.
- **Spuštění gRPC serveru:** viz README, sekce „Orchestrator gRPC server“.
