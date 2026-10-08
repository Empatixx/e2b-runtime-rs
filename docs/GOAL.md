# Cíl: orchestrator M9 → M10 → M11 → M12 bez dozoru

Pracuješ sám a nikdo nic neschvaluje. Tenhle dokument je zadání i schválení. Plány M9–M12 si schválíš sám. Kde dokument mlčí, rozhodni, zapiš `Ruling:` do ledgeru a pokračuj. Pořadí je pevné. Lepší pořádně hotové M9 a M10 než čtyři rozdělané milníky.

Jde o přepis upstream E2B z Go do Rustu: business logika, limity, výchozí hodnoty, stavové kódy a texty chyb jako upstream. Struktury a styl ale podle našich konvencí, stejně jako u envd.

## Stav

- **Repo:** `~/IdeaProjects/envd-rs`, větev `main`.
- **Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`.
- **Výchozí čísla (M8):** resume 1,5–2,1 s, pause 556–705 ms.

## Hotovo znamená (vše zelené v CI)

- **M9:** cache šablon, pool síťových slotů, pool NBD a souběžné NBD dispatch, souběžný resume. `resume_contract` vypíše rozpad fází.
- **M10:** `InfoService`, `/health` na stejném portu, admission limity, graceful shutdown s drainem.
- **M11:** Pause vrátí odpověď před uploadem, `AddSnapshot`, upload na pozadí s retry, `DiffStore` s TTL a vyklízením podle místa na disku.
- **M12:** streamovaný chunker, trackery, prefetch ze záznamu, optimize, memfd.

U každého milníku zapiš čísla před a po do `docs/NIGHT-REPORT.md`.

**Na konci:**
1. Poslední CI běh je zelený.
2. `docs/NIGHT-REPORT.md` je česky přepsaný a pushnutý.
3. Ledgery hotových plánů jsou smazané.

## Jak pracovat

- **Plán:** každý milník má plán v `docs/superpowers/plans/`. Implementace jde úkol po úkolu, test napřed a ověř, že padá. Commit po každém úkolu, ledger je v `.superpowers/sdd/<plán>/progress.md`.
- **Před commitem:** `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. Příkazy řetězit jen přes `&&`.
- **Linux a CI:** linuxové věci se ověřují v GitHub Actions. Push na `origin main` kvůli CI je povolený.
  - Běh sleduj přes `gh run view`.
  - Síťový výpadek zkus jednou znovu přes `gh run rerun --failed`.
  - Testy nikdy nezmírňuj. Na nejasné chyby použij systematic-debugging.
- **Review:** na konci milníku review na opusu. Kritické a důležité nálezy oprav s testem, který napřed padá. Drobnosti patří do `docs/BACKLOG.md`.
- **Styl podle `CLAUDE.md`:**
  - malé metody, řetězení, buildery, pojmenovaná pole;
  - žádné komentáře, `.0` ani `#[allow]`;
  - `unsafe` a syscally schované v malých typech.
- **Upstream:** `~/IdeaProjects/e2b-runtime-origin-main-review/packages`. Větší průzkum deleguj na agenta `Explore`.

## Zakázáno

- `cargo clean`, ruční zásahy do `target/`, `git stash`, přepínání větví, worktree.
- Force-push, přepis pushnuté historie, PR a merge.
- Jakýkoli `kubectl`, hlavně proti `omni-legal-machine-prod`.
- Mazání cizích Docker kontejnerů, image, volume a sítí.

## Kdy se zastavit

- Další krok je nevratný nebo zakázaný.
- Plán je tak rozbitý, že každá cesta by byla hádání.
- Stejná chyba v CI zůstává po 3 různých hypotézách. Zapiš je a pokračuj jinou nezávislou prací.
