# Cíl: dokončit orchestrator M6 → M7 → M8 bez dozoru

Pracuješ sám, nikdo nic neschvaluje. Tenhle dokument je zadání i schválení, plány M7 a M8 si schválíš sám. Kde dokument mlčí, rozhodni sám, zapiš `Ruling:` do ledgeru a pokračuj. Pořadí je pevné. Lepší pořádně hotové M6 a M7 než tři rozdělané milníky.

## Stav

- **Repo:** `~/IdeaProjects/envd-rs`, větev `main`.
- **Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`.
- **Plán M6:** `docs/superpowers/plans/2026-10-10-orchestrator-m6-resume.md`.
- **Úkoly 1–3 M6** jsou commitnuté. Pushnutý a zelený v CI je jen úkol 1, úkoly 2–3 jsou zatím jen lokálně.
- **Úkol 4** (omezené pokusy `/init`, závod proti pádu Firecrackeru, MMDS před startem) je napsaný, necommitnutý a na Linuxu nepřeložený. Pushni ho a oprav podle CI.
- **Zbývají úkoly 5–9 M6.**

## Hotovo znamená (vše zelené v CI)

**M6 – resume:**
- `resume_contract`: soubor zapsaný před snapshotem je po resume vidět a test vypíše `resume took N ms`.
- Oficiální SDK testy projdou i proti obnovenému sandboxu.

**M7 – pause:**
- dirty stránky paměti a diffy paměti i rootfs nad rodičovskými headery, upload a nový build (upstream `Sandbox.Pause`, `fc/memory.go`, `build_upload*`);
- test: studený start → soubor A → pause → resume → soubor B → pause → resume, pak jsou vidět A i B a proces na pozadí běží dál;
- vypsat velikost diffu.

**M8 – gRPC:**
- `SandboxService` z upstream `orchestrator.proto` (tonic): Create, Delete, Pause, List, Update;
- mapa sandboxů, timeouty s úklidem, pool slotů, binárka `orchestrator`;
- z backlogu: sledování smrti NBD a uffd serveru, `Drop` pro `NbdDevice`, `setsid`, cgroup controllers;
- test přes skutečný server: Create → List → Pause → Create z buildu → Delete, a nic nezůstane.

## Jak pracovat

- **Plán:** každý milník dostane plán v `docs/superpowers/plans/` ve stylu M5 a M6.
- **Implementace:** úkol po úkolu, test napřed, sleduj, že padá. Commit po každém úkolu, ledger v `.superpowers/sdd/<plán>/progress.md`.
- **Před commitem:** `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. Commit jen po zeleném běhu, takže řetězit přes `&&`, nikdy přes `;`.
- **Linux a CI:** linuxové věci (KVM, nbd, uffd) se ověřují v GitHub Actions. Push na `origin main` kvůli CI je povolený.
  - Běh sleduj přes `gh run view <id>`.
  - Síťový výpadek GitHubu zkus jednou znovu přes `gh run rerun --failed`.
  - Testy nikdy nezmírňuj. Při nejasné chybě použij systematic-debugging.
- **Review:** na konci milníku review na opusu (`Agent`, `model: opus`).
  - Kritické a důležité nálezy oprav, každý s testem, který napřed padá.
  - Drobnosti zapiš do `docs/BACKLOG.md`.
- **Styl (`CLAUDE.md`):**
  - malé metody, řetězení, buildery, pojmenovaná pole;
  - žádné komentáře, `.0` ani `#[allow]`;
  - `unsafe` a syscally schované v malých typech;
  - parita s upstream Go tam, kde ji klient vidí. Upstream: `~/IdeaProjects/e2b-runtime-origin-main-review/packages`, větší průzkum deleguj na agenta `Explore`.

## Zakázáno

- `cargo clean`, ruční zásahy do `target/`, `git stash`, přepínání větví, worktree. Ve stejném repu může pracovat i jiná session.
- Force-push, přepis pushnuté historie, PR a merge.
- Jakýkoli `kubectl`, hlavně proti `omni-legal-machine-prod`.
- Mazání cizích Docker kontejnerů, image, volume a sítí.

## Kdy se zastavit

- Další krok je nevratný nebo zakázaný.
- Plán je tak rozbitý, že každá cesta dál by byla hádání.
- Stejná chyba v CI zůstává po 3 různých hypotézách. Zapiš je a pokračuj jinou nezávislou prací, pokud nějaká je.

## Na konci

1. Všechno commitni a pushni. Poslední běh CI musí být zelený.
2. Napiš česky `docs/NIGHT-REPORT.md` a pushni ho:
   - co je hotové a čísla běhů CI;
   - doba resume a pause;
   - všechna rozhodnutí `Ruling:`;
   - co zůstalo otevřené a proč.
3. Smaž ledgery hotových plánů.
