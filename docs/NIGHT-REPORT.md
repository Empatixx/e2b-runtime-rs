# Report: orchestrator M9 → M12

M9 až M12 jsou hotové a poslední běh CI je zelený. Je to přepis upstream E2B logiky z Go do Rustu: limity, výchozí hodnoty, stavové kódy a texty chyb odpovídají upstreamu, struktury a styl jsou podle našich konvencí. Po M9–M11 a po M12 proběhlo review na opusu. Důležité nálezy jsou opravené, drobnosti jsou v `docs/BACKLOG.md`.

Předchozí report (M6–M8) je v historii gitu.

## Čísla (CI runner, 1 GiB guest)

| Co | Před (M8) | Teď |
|---|---|---|
| Resume s prefetchem ze záznamu (optimize) | — | **463 ms** (`envd_init` 251 ms, 460 faultů, 4551 stránek předem) |
| Resume bez prefetche, teplá cache chunků | — | 872 ms (5408 faultů) |
| Resume, studená cache | 1,5–2,1 s | 1,5 s |
| Pause přes gRPC | 556–705 ms (včetně uploadu) | **84–217 ms** (upload běží na pozadí) |
| Resume na stejném uzlu hned po pause | stahovalo se všechno ze storage | čte jen lokální soubory, ~2 s bez záznamu |

**Kde mizel čas.** Baseline v M9 ukázala, že síť a rootfs zaberou jen ~130 ms. Zbytek, 1,4–1,9 s, zabíralo `envd_init`. Šlo hlavně o tisíce page faultů, kdy guest po resume dotahuje paměť. Proto přinesl největší zrychlení prefetch ze záznamu, ne pool slotů.

Rozpad jednotlivých fází vypisuje `resume_contract` (`resume phases: …`).

## Co je hotové

### M9: rychlý resume

- **Cache šablon** (`template/`): snapfile a headery se stahují jednou na build a drží se 25 h s klouzavým TTL. „Pin“ je počet `Arc` referencí plus 1 min grace po posledním uvolnění.
- **Pool síťových slotů** (`network/pool.rs`):
  - 32 nových a 100 recyklovaných slotů, vrácení se zpožděním 3 s;
  - indexy přeskakují existující `ns-N` a uniklé namespacy se použijí až jako poslední;
  - namespace zůstane jako kotva, když teardown selže.
- **Pool NBD zařízení** (64 připravených) a souběžné zpracování NBD požadavků. Chyba backendu vrací kód 1 a zápis nad 32 MB server ukončí, obojí jako upstream.
- **Souběžný resume:**
  - Firecracker startuje zároveň s připojením NBD, rootfs je symlink na `/dev/null` a později se přepojí;
  - cgroup se přiřadí už v `pre_exec` (obdoba `CLONE_INTO_CGROUP`);
  - žádný `PUT /logger`; nastavují se prázdné rate limitery;
  - envd `/init` zkouší po 50 ms s celkovým limitem 10 s.
- **Měření fází** (`phases.rs`).

### M10: služby uzlu

- **`InfoService`** (`info.proto` beze změn): stav uzlu, metriky hostitele a součty prostředků přidělených sandboxům. Override stavu má stejná pravidla a texty chyb jako upstream.
- **Jeden port pro gRPC, gRPC health a `GET /health`** (axum místo cmux). `/health` vrací `{"status":"healthy|draining|unhealthy","version":"<commit>"}`, kód 503 jen pro unhealthy.
- **Admission:** max 200 sandboxů a 3 souběžné starty s čekáním 15 s, texty chyb jako upstream.
- **Shutdown jako upstream:**
  1. stav ShuttingDown;
  2. 15 s pauza;
  3. drain, tedy čekání, až API sandboxy odebere;
  4. dokončení uploadů;
  5. teprve pak zavření poolů.
  - Druhý SIGTERM sandboxy smaže.

### M11: asynchronní pause

- **Pause** uloží snapshot do cache šablon (`AddSnapshot`) a odpoví hned. Upload běží na pozadí: 20 min na pokus, 2 h celkem, prodleva mezi pokusy 5 s zdvojnásobovaná až do 2 min.
- **Pořadí uploadů:** potomek je označený jako nahraný až po rodiči.
- **Selhaný upload** nechá build v cache, kde se dá dál resumovat na tomto uzlu, dokud nevyprší TTL.
- **`DiffStore`:** TTL 25 h, lokální diffy připíchnuté do konce uploadu, vyklízení při zaplnění disku přes `BUILD_CACHE_MAX_USAGE_PERCENTAGE` (výchozí 90).

### M12: prefetch

- **`metadata.json`** se stejnými klíči jako upstream, s fallbackem na V1. Pause zapisuje `same_version` a metadata nahrává.
- **Záznam faultů:** u každé stránky se zaznamená první dotek, pořadí a typ přístupu (`r`/`w`/`p`).
- **`collect_prefetch`:** průnik dvou resumů jako upstream optimize.
- **Prefetcher:** 16 workerů stahuje stránky a 8 je kopíruje přes `UFFDIO_COPY` s write-protect. Stahování začíná hned na začátku resume, kopírování až po handshaku.
- **Streamovaný chunker:** čtenář čeká jen na svoje bloky. Zkrácený stream neoznačí žádnou stránku jako hotovou.
- **memfd:** Firecracker 1.14.1 ho posílá, takže pause kopíruje dirty stránky z memfd místo `process_vm_readv`.

## Rozhodnutí, která jsem udělal za tebe (Ruling)

**Síť a NBD:**
- Pool slotů po chybě čeká 50 ms (upstream nečeká), aby rozbitý host nezatěžoval CPU.
- Sloty se dál vytvářejí přes `ip` a `iptables`, protože po zavedení poolu jsou mimo rychlou cestu. Přechod na netlink je v backlogu.
- `Slot::create` přes existující namespace ho nejdřív smaže, jako upstream `CreateNetwork`.
- NBD dál používá ioctl a `SEND_FLUSH`, protože flush synchronizuje rootfs před exportem. Multi-conn proto nemáme.
- Pool NBD zařízení je jeden na proces a běží na std vláknech. Testy mají každý vlastní tokio runtime. Úloha poolu v jednom runtime by skončila spolu s testem.
- Čekání na připojení NBD používá `sleep(100 µs)`, který tokio zaokrouhlí zhruba na 1 ms.

**Resume a envd:**
- Výchozí timeout envd v knihovně zůstává 60 s, protože studený boot systemd guestu v CI to potřebuje. `Node` předává `ENVD_TIMEOUT_MS` (10 s).
- MMDS `address` zůstává prázdná, dokud nebude hyperloop.

**Node a API:**
- `memory_used` počítá sysinfo jako total − available, gopsutil to dělá trochu jinak.
- Deprecated pole ServiceInfo 101–103 zůstávají 0, protože `#[allow]` je zakázané.
- Limit 60 s na Create omezuje čekání (na slot), ale běžící resume neruší, aby neunikly prostředky z poolů.
- **SIGTERM už sandboxy nemaže**, drainuje jako upstream. Mažou se až druhým signálem. To nahrazuje rozhodnutí z M8.

**Pause a upload:**
- Headery V3 jdou nahoru souběžně s daty a na rodiče se čeká až před označením „nahráno“, stejně jako v upstream.
- `BUILD_CACHE_MAX_USAGE_PERCENTAGE` má výchozí hodnotu 90.

**Prefetch:**
- Verze kernelu a Firecrackeru v `metadata.json` se kopírují z rodiče.
- `RESUME_PREFETCH_SOURCE` umí jen `off` a `init`. `last-cycle` je v backlogu.
- Stream chunků jde po dávkách 256 KiB.

**Review:**
- Pro M9–M11 proběhlo jedno společné review místo tří, protože milníky sdílejí kód uzlu.

## Testy, které jsem neviděl nejdřív padat

Linuxový kód lokálně nepřeložím, takže opravy jako unpin po selhaném uploadu nebo pořadí uploadu před odpovědí na pause ověřilo až CI.

Naopak skutečně padaly a pak prošly:
- zkrácený stream;
- tolerantní `metadata.json`;
- prefault bez write-protect;
- tři testy, u kterých byla chyba v samotném testu: velikost NBD zařízení, ping blokovaný pravidlem hostitele a nevrácený slot.

## Co zůstává

- **Template builder (navrhuju jako M13):** OCI image → ext4 → provisioning → snapshot → optimize. Bez něj vznikají buildy jen v testech a záznam faultů pro prefetch nemá kdo zapsat do `metadata.json` produkčního buildu. `collect_prefetch` je na to připravený.
- **Backlog:**
  - netlink a nftables firewall;
  - generic-netlink NBD;
  - `last-cycle` prefetch a harvest po pause;
  - huge pages;
  - peer-to-peer chunky;
  - drobnosti z obou review.
- **M3b ublk** pořád čeká na bare-metal node.

## Kde to je

- Větev `main` v `Empatixx/e2b-runtime-rs`, všechno pushnuté.
- **Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`.
- **Plány:** `docs/superpowers/plans/2026-10-13-…-m9-…` až `2026-10-16-…-m12-prefetch.md`.
