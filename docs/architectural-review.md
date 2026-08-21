# Architectural review: SC2CCM (old-reference)

Working document for the StarVault CCM rebuild. Everything is grounded in the
actual code of the original SC2 Custom Campaign Manager by 7thAce &
GiantGrantGames (v1.03–v1.04, C# / .NET Framework 4.7.2 / WinForms).

## Verdict

This is a domain-knowledge asset wrapped in an architecture not worth carrying
forward. The author understood the problem deeply: the SC2 install directory is
a fixed contract, campaigns are just file trees, and "switching" means swapping
trees. That insight is encoded correctly all over the codebase. But all
business logic lives in two parallel 1000-line WinForms code-behinds, one of
which is dead. The three-project solution split looks like layering but isn't.
The updater chain has a naming mismatch that likely breaks it in production.

Mine it for behavior and edge cases. Do not mine it for structure.

## The system in one diagram

```
                 ┌────────────────────────────┐
   GitHub API ──▶│ Updater.cs (in CCM)        │──▶ downloads exe,
                 └────────────┬───────────────┘    spawns CCMUpdater
                              ▼                    │
┌─────────────────────────────────────────────┐    ▼
│ FormMain (WinForms code-behind, ~1000 loc)  │  CCMUpdater/
│  · SC2 path discovery (registry + txt)      │  swaps exe files
│  · zip import/extraction                    │  while main exits
│  · dependency hoisting                      │
│  · campaign switch/delete/restore           │
│  · UI state, logging, prompts               │
└───────┬──────────────────┬──────────────────┘
        │ uses             │ uses
        ▼                  ▼
┌──────────────────┐  ┌──────────────────────┐
│ Base: Mod,       │  │ Services: ZipService,│
│ Campaign enum    │  │ ZipArchiveExtensions │
└──────────────────┘  └──────────────────────┘
        both write directly to:
        <SC2>\Maps\... and <SC2>\Mods\   ◀── the real database
```

The SC2 game itself is the consumer of everything this app writes. That is the
architectural fact the whole design hangs on, and it is a good one.

## The core idea

The app keeps no state of its own worth mentioning. Which campaign is active is
*derived*, never stored: a `metadata.txt` sitting in `Maps\Campaign\swarm` means
HotS has a custom campaign; its absence means default Blizzard files. Config is
one line in `%APPDATA%\SC2CCM\SC2CCM.txt` (path to the exe). Version state is
two lines in a second txt file.

Filesystem-as-truth means a crash mid-anything cannot corrupt a database,
because there is no database. The cost shows up elsewhere: no integrity
checking (a half-copied slot just looks like a broken campaign), and every
"what's active" question costs a directory scan plus a metadata parse
(`setInfoBoxes()`, FormMain.cs:404).

## Layering audit

The solution *has* three projects. It does not have three layers.

- **Base** holds `Mod` (a bag of strings) and the `Campaign` enum. Parsing
  logic doesn't live here. `processLine()` lives in the form, twice
  (Form1.cs:521, FormMain.cs:480), with drift between copies.
- **Services** holds exactly one service, `ZipService`, which reaches into the
  SC2 directory layout directly (hardcoded `Maps\CustomCampaigns`,
  `Maps\Campaign\voidprologue`). Path knowledge leaked into a second place.
- **UI** holds everything that matters: install, switch, delete, restore,
  dependency hoisting, path discovery, config IO. None of it callable without
  a Form.

The one genuine seam is `new ZipService(logBoxWriteLine)` (FormMain.cs:31),
injecting the logger as `Action<string>`. That is the pattern the rebuild
generalizes.

Worse than the missing layers: there are two full implementations of the app.
`Form1.cs` compiles nothing (not in the csproj) but carries an older copy of
every operation, written against a `Mod` API with `GetTitle()`/`SetPath()`
methods that no longer exist. The copies already disagree on real behavior:
Form1's LotV set-handler moves `voidprologue` subfolders at set time
(Form1.cs:757); FormMain's doesn't, because `ZipService` now does it at unzip
time. Dead code that still documents divergent behavior is worse than no dead
code, because the next reader can't tell which behavior ships.

## Domain model audit

`Mod` mixes auto-properties and public fields, carries a dead `LotVprologue`
property, and infers the target campaign by substring-matching the metadata
value: `wings|liberty|wol`, `heart|swarm|hots`, `legacy|void|lotv`,
`nova|covert|ops|nco` (Mod.cs:15). First match wins in that order.

Fuzzy matching is the right call for this ecosystem. Metadata files are
handwritten by amateur mappers, and strict enums would generate support
threads. But substring matching on `ops` and `void` will misfire eventually,
and when it does, the mod lands in bucket 0, which no dropdown reads. Silent
invisibility is the worst failure mode available here.

The other modeling smell is `List<Mod>[5]` indexed by enum cast, flagged in a
comment at FormMain.cs:21 ("should be Dictionary"). The deeper coupling is that
dropdown item order must equal list order for selection indexing to work
(FormMain.cs:591) — an invariant maintained by accident, documented nowhere.

## State, invariants, failure semantics

Three invariants hold the system together, none enforced:

1. Every top-level folder under `CustomCampaigns\` contains a `metadata.txt`.
   Violations get logged and skipped.
2. No `.SC2Mod` remains under `CustomCampaigns\` after load. Enforced
   destructively: `handleDependencies()` *moves* dependencies out to `Mods\`
   permanently.
3. Each active slot contains either default files or exactly one campaign's
   files. Best effort via `clearDir()` before copy.

Invariant 2 is the design decision to reverse first. Moving dependencies out of
the library folder means: deleting a campaign orphans its deps forever; two
campaigns sharing a dependency name clobber each other silently, last import
wins; the stored package is no longer self-contained.

Failure semantics are uniformly "log a string and continue." A locked file
during a switch leaves a half-cleared slot; recovery is a message telling the
user to exit SC2 and hit Reload. There's no staging, no rollback, no
verification pass after copy. For a tool whose entire job is moving files, the
absence of a transaction boundary around "move files" is the central
engineering gap.

One latent correctness bug: `copyFilesAndFolders()` maps paths with
`dirPath.Replace(sourcePath, targetPath)` (FormMain.cs:498, 505). If the source
path string recurs inside a deeper path, the copy corrupts.

Also: `importFiles()` deletes the user's source folder after copying it
(FormMain.cs:903). Drag a folder in, watch it vanish from where you dragged it.

## Responsiveness

Every operation runs synchronously on the UI thread. Copying a
multi-hundred-megabyte campaign freezes the window with no progress indication.

## Security

Good news first: zip extraction checks for path traversal and refuses to escape
the destination (ZipArchiveExtensions.cs:24). Someone knew about zip-slip and
handled it.

Weaker spots: the updater downloads executables from GitHub releases with no
hash or signature verification; rate limiting is detected by sniffing the
response body for a phrase (Updater.cs:47); `WebClient` is long deprecated.

## Testability and observability

Zero tests, zero seams to hang them on. The logic is pure filesystem
transformation, which makes it almost embarrassingly testable once extracted:
point a `SlotManager` at a temp directory and assert on the tree. Observability
is a rich-text box and leftover `Console.WriteLine("SUBDIRVISIONS: ...")`
calls.

## Debt register

| Severity | Item | Where |
|---|---|---|
| Critical | All install/switch/delete logic trapped in Form code-behind | FormMain.cs |
| Critical | Destructive, unrefcounted dependency hoisting | handleDependencies(), FormMain.cs:177 |
| Critical | Non-atomic switch operations, no rollback | wolSetButton_Click et al. |
| High | Duplicate divergent implementation shipped as dead code | Form1.cs (entire) |
| High | Updater exe-name mismatch (`StarCraft 2 ...` vs `SC2 ...`) | csproj vs CCMUpdater/Program.cs:22 |
| High | Path.Replace path mapping | FormMain.cs:498 |
| Medium | Import deletes source folders | FormMain.cs:903 |
| Medium | `None`-tagged mods silently invisible | populateDropdowns() |
| Medium | Dropdown-order/list-order implicit invariant | FormMain.cs:591 |
| Medium | Synchronous IO on UI thread | everywhere |
| Low | Config as first-line-of-txt, registry probing fallback | findSC2() |
| Low | Commented-out blocks, debug prints, TODOs | throughout |

## Keep, kill, build

**Keep.** The `metadata.txt` contract byte for byte, including fuzzy campaign
matching. Thousands of existing zips depend on it; compatibility with them is
the product. The four-slot directory model and prologue special cases — they're
dictated by the game. Filesystem-derived active state. Zip-slip protection.
Tolerant parsing.

**Kill.** Dependency hoisting as a move; replace with content-addressed storage
plus refcounts so delete cleans up and shared deps survive. The dead form.
String-path plumbing in favor of a single module that owns the directory
layout.

**Build.** A core assembly with pure, testable types: PackageParser, Library,
SlotManager (staged transactions), DependencyStore. Then a thin shell over it.

The original README states the goal as "the user never having to open the file
explorer." The path there runs through making switching reliable enough to
trust blindly — exactly what the missing transaction boundary costs the
original design.
