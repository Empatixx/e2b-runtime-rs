# Jak funguje e2b-runtime-simple

Celé to stojí na jednom pojmu: **checkpoint** je stav CPU a seznamy vrstev (disk, paměť, volumes). Vrstvy se jmenují podle sha256, leží v S3 a node je má v cache. Create, pause, resume i snapshot jen vyrobí checkpoint, nebo z něj spustí VM.

English version: [how-it-works.md](how-it-works.md).

## 1. Celkový obrázek

```
                        ┌──────────────┐
                        │   E2B SDK    │
                        └──┬────────┬──┘
           API (create,    │        │   provoz sandboxu (envd: příkazy, soubory, porty)
           pause, connect) │        │   http://49983-<id>.<doména nodu>
                           ▼        ▼
          ┌─────────────────┐    ┌─────────────────┐
          │     node A      │◀──▶│     node B      │   stejný binár e2b-node
          │ API  :3000      │    │ API  :3100      │   cizí volání předá vlastníkovi
          │ proxy:3001      │    │ proxy:3101      │
          │ VM, VM, VM      │    │ VM, VM          │
          │ lokální cache     │    │ lokální cache     │
          └────────┬────────┘    └────────┬────────┘
                   └───────────┬──────────┘
                               ▼
                    ┌─────────────────────┐
                    │  S3 (jediná sdílená │  vrstvy + JSON záznamy
                    │   věc mezi nody)    │
                    └─────────────────────┘
```

## 2. Uvnitř jednoho nodu

```
 e2b-node (root)
 ├─ api/            E2B REST: /v2/sandboxes, /pause, /connect, /snapshots, /volumes
 │   └─ forward.rs  "běží jinde?" pak request pošli vlastníkovi
 ├─ proxy.rs        provoz do guestu: hlavička E2b-Sandbox-Id, IP slotu, envd
 ├─ sandbox.rs      mapa BĚŽÍCÍCH sandboxů (pauznuté jsou jen v S3)
 ├─ warm_pool       4 Firecrackery spuštěné dopředu, každý se svým síťovým slotem
 ├─ shared_memory   1 read-only paměťové zařízení na checkpoint, sdílené
 └─ spouští ─▶ uvm-ublk-daemon (z AgentENV)
                 └─ /dev/ublkbN = disky a paměť složené z vrstev OverlayBD
```

## 3. Vrstvy: jak se skládá disk

OverlayBD čte shora dolů a vrátí první vrstvu, která daný blok má.

```
 guest vidí /dev/vda (ext4)
        │
        ▼
 /dev/ublkb7 ── OverlayBD ──┐
                            │  čte shora:
   ┌────────────────────────┴───────────────┐
   │ runtime upper  (lokální, ZAPISOVATELNÁ)  │ ← sem jdou všechny zápisy sandboxu
   ├─────────────────────────────────────────┤
   │ sha256:d623…  změny z pause #1          │ ┐
   ├─────────────────────────────────────────┤ │ read only, sdílené
   │ sha256:9629…  ext4 z Docker image       │ ┘ všemi sandboxy
   └─────────────────────────────────────────┘
          │ vrstva v lokální cache?
          ├─ ano: čte soubor data/layers/sha256:….commit
          └─ ne:  čte s3://bucket/layers/sha256:…  (po blocích, pak v cache)
```

**Paměť** funguje stejně, jen read only. Firecracker si paměťové zařízení namapuje jako soubor:

```
 guest RAM (1 GiB)
   │ čtení stránky   → page cache /dev/ublkbM  (sdílená všemi sandboxy z checkpointu)
   │ zápis stránky   → soukromá kopie v procesu Firecracker (copy on write)
   ▼
 /dev/ublkbM ── OverlayBD ── [ paměť z pause #1 ] [ paměť template (74 MB z 1 GiB) ]
```

## 4. Create

```
 SDK            node A                         S3                 Firecracker + guest
  │ POST /v2/sandboxes {templateID: base}
  │──────────────▶│
  │               │ templates/base.json ──────▶│ → checkpoint ckpt-x
  │               │ checkpoints/ckpt-x.json ──▶│ → seznamy vrstev, vmstate
  │               │ vezmi Firecracker z warm poolu (už běží, slot hotový)
  │               │ rootfs: zapisovatelné ublk zařízení z poolu daemonu (~1 ms)
  │               │ paměť: sdílené zařízení checkpointu
  │               │ PUT /snapshot/load ─────────────────────────────▶│ VM běží (~5 ms)
  │               │ MMDS: id sandboxu, hash tokenu ─────────────────▶│
  │               │ GET /health, POST /init ────────────────────────▶│ envd (~50 ms, page faulty)
  │               │ sandboxes/<id>.json {owner: A} ─▶│
  │◀──────────────│ 201 {sandboxID, domain: "node-a…", envdAccessToken}
  │                                                         celkem asi 58 ms
```

## 5. Pause

```
 SDK           node A                                        S3
  │ POST /sandboxes/<id>/pause
  │─────────────▶│ PATCH /vm Paused            (vCPU stojí)
  │              │ PUT /snapshot/create        → vmstate (25 KB)
  │              │ GET /vm/dirty-memory-ranges → jen zapsané stránky
  │              │   process_vm_readv → vrstva paměti ─────────────▶│ layers/sha256:…
  │              │ fsync + restack rootfs      → horní vrstva se zapečetí ▶│ layers/sha256:…
  │              │ (volumes: totéž)                              │
  │              │ checkpoints/ckpt-y.json + vmstate ──────────────▶│
  │              │ sandboxes/<id>.json {owner: none, ckpt-y} ──────▶│
  │◀─────────────│ 204                                asi 34 ms
  │              │ na pozadí: kill Firecrackeru, vrátit zařízení a slot
```

Po pause sandbox nepatří žádnému nodu a celý jeho stav je v S3.

## 6. Resume na jiném nodu

```
 SDK           node B                                         S3
  │ POST /v2/sandboxes/<id>/connect
  │─────────────▶│ GET sandboxes/<id>.json  (etag "e1") ──────▶│ {owner: none, ckpt-y}
  │              │ PUT sandboxes/<id>.json  If-Match: "e1" ───▶│ {owner: B}   ✅
  │              │      (kdyby mezitím zapsal node C: 412, "resumuje se jinde")
  │              │ GET checkpoints/ckpt-y/vmstate ────────────▶│ (25 KB)
  │              │ vrstvy: co B nemá v cache, čte OverlayBD z S3 podle potřeby
  │              │ load snapshot, /init
  │◀─────────────│ 201 {domain: "node-b…"}            asi 125 ms
  │
  │ dál SDK mluví rovnou s nodem B (49983-<id>.node-b…)
```

API volání na sandbox, který běží jinde:

```
 SDK ── DELETE /sandboxes/<id> ──▶ node A
                                    │ není můj, záznam říká owner B
                                    │ předej (hlavička x-e2b-simple-forwarded)
                                    └──────────────▶ node B ── kill ── 204
 SDK ◀──────────────── 204 ─────────┘                         asi 3 ms
```

## 7. Síť jednoho sandboxu

```
 ┌─ netns e2bs-7 ─────────────────────────────────────────────┐
 │                                                            │
 │  guest eth0 169.254.0.21 ═══ tap0 169.254.0.22             │  každý guest má STEJNOU IP
 │                                   │                        │  (kvůli snapshotům)    
 │        NAT: 169.254.0.21 ⇄ 10.11.0.7  (IP slotu)             │
 │                                   │                        │
 │                              eth0 10.12.0.30 ══ veth ══╗   │
 └────────────────────────────────────────────────────────╫───┘
                                                          ║
  host:  e2bv7 10.12.0.29 ◀════════════════════════════════╝
         route 10.11.0.7 přes veth slotu 7
         proxy nodu ──▶ 10.11.0.7:49983 ──▶ envd v guestu
         ven: MASQUERADE do internetu ✅
         blokuje: jiné sandboxy (10.11/16), host (INPUT z e2bv+), privátní sítě, Azure 168.63.129.16 ❌
```

## 8. Volumes (například `/home/user/data`)

```
 template má 2 rezervované sloty: vdb, vdc, normálně ukazují na prázdný placeholder

 create s volume:
   volumes/<vol>.json  If-Match: attached_to = <sandbox>   (exclusive, vždy jen jeden sandbox)
   vol0.dev ──symlink──▶ /dev/ublkb9 (vrstvy volume)
   load snapshot, PATCH /drives/vol0 (guest se dozví novou velikost), envd: mount /dev/vdb

 kill:
   sync v guestu, pause, zapečetit horní vrstvu volume ─▶ S3 layers/sha256:…
   volumes/<vol>.json { layers += nová, attached_to: none }
   další sandbox připojí volume na libovolném nodu
```

## 9. Build template

```
 Docker image ubuntu:24.04
   + useradd user, sudo, python3, envd, /sbin/e2b-init
   │ docker build, docker export
   ▼
 adresář → mkfs.ext4 -d → rootfs.ext4 → vrstva OverlayBD ────────────▶ S3 layers/sha256:9629…
   │
   ▼ cold boot Firecrackeru z té vrstvy, počkat na envd, 2 s klidu
   │ capture (jako pause): paměť + změny disku + vmstate ───────────────▶ S3
   ▼
 templates/base.json → ckpt-x                         asi 7 s s teplou cache Dockeru
```

## 10. Co je v S3

```
 s3://e2b-simple/
 ├─ layers/sha256:9629…            160 MB  ext4 z image          ┐
 ├─ layers/sha256:e646…             74 MB  paměť template        │ immutable,
 ├─ layers/sha256:d623…             20 KB  změny disku           │ podle obsahu
 ├─ layers/sha256:…                        každá pause, volume   ┘
 ├─ checkpoints/ckpt-x.json                { rootfs_layers, memory_layers, drives, vcpu, mem }
 ├─ checkpoints/ckpt-x/vmstate      25 KB  stav CPU a zařízení
 ├─ templates/base.json                    → ckpt-x
 ├─ volumes/vol-….json                     { layers, attached_to }
 └─ sandboxes/i….json                      { info, owner: {node, api_url, domain} | none, checkpoint }
```

Lokální disk nodu je jen cache, takže jeho ztráta nic nerozbije:

```
 data-a/layers/sha256:….commit   cache vrstev (hardlinky na zapečetěné soubory)
 data-a/checkpoints/<id>/vmstate cache vmstate
 data-a/sandboxes/vm-…/          běžící VM: horní vrstva disku, fc.sock, symlinky
 data-a/memory/<ckpt>/           image.json sdíleného paměťového zařízení
```

## 11. Úklid

### Node, který spadne

```
 každý node, každých 5 s:   PUT nodes/node-a.json { alive_until: teď + 30 s }

 node A čte sandboxes/<id>.json { owner: node-b }
    └─ nodes/node-b.json alive_until < teď ?
          ne  → předá volání na B (jako dřív)
          ano → B je pryč:
                 volumes připojené k sandboxu → attached_to: nikdo (If-Match)
                 smazat checkpoint, ze kterého byl obnovený
                 smazat záznam → SDK dostane 404

 node B se restartuje → nejdřív zapomene svoje záznamy (jeho VM restart nepřežily)
```

Naměřeno: hned po `kill -9` nodu B vrací volání na jeho sandbox 502. Po 37 s vrací 404, sandbox zmizí ze seznamu a jeho volume jde připojit na nodu A.

### Slučování vrstev

```
 rozpočet vlastních vrstev = 32 - sdílené / 4   (minimálně 1, pravidlo z AgentENV)

 41. pause, 3 sdílené vrstvy šablony:      po sloučení:
 ┌───────────────────────┐                 ┌───────────────────────┐
 │ pause #33             │ ┐               │ sloučená sha256:86cf… │ ← jedna nová vrstva v S3
 │ …                     │ │ 33 vlastních  ├───────────────────────┤
 │ pause #1              │ ┘ > 32          │ vrstvy šablony        │ ┐ sdílené,
 ├───────────────────────┤                 │ (nikdy se neslučují)  │ ┘ beze změny
 │ vrstvy šablony        │                 └───────────────────────┘
 └───────────────────────┘
```

Vlastní vrstvy, které jsou jen v S3 (po resume na jiném nodu), se nejdřív stáhnou do cache. `examples/sdk-smoke/many-pauses.mjs` udělá 40 kol zápisu, pause a resume; na konci má kořenový disk 11 vrstev a všechny soubory jsou na místě.

### Staré checkpointy a lokální cache

```
 pause:  nový checkpoint uložen → smazat checkpoint, ze kterého byl sandbox obnovený
 kill:   smazat poslední checkpoint pauznutého sandboxu
 vrstvy: zůstávají v S3 (sdílí je šablony a další checkpointy), stejně jako v AgentENV

 lokální cache, každých 30 min:  nad 95 % E2B_SIMPLE_CACHE_GB?
                                 → smazat nejdéle nepoužité vrstvy, nečinné 10 min, až na 70 %
                                 (každá vrstva je i v S3, takže se nic neztratí)
```
