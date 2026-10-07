# Shared skills

Open Settings in the GPUI app, or press `cmd-,`, then select Skills.

Import a folder containing `SKILL.md`. MonoCode copies the complete folder into its library and records the original location. Importing the same name and content again reuses its entry. A different bundle with the same name causes a conflict. Rename the skill before importing that bundle.

Open the source folder to edit instructions, scripts, or references. Apply edits validates the folder and saves a new revision. MonoCode updates copies that it still owns. Start a new provider session to load a changed skill.

The Library list shows managed entries. Existing shows discovered project and personal skills, including duplicate locations. Importing an existing skill preserves its original files.

Sharing uses these directories on the local host:

| Providers | Managed directory |
| --- | --- |
| Codex, Cursor, Grok, OpenCode, Pi, fx, Droid | `~/.agents/skills` |
| Claude | `~/.claude/skills` |
| OMP | `~/.omp/agent/skills` |
| Hermes | `~/.hermes/skills` |

Named Claude and Codex accounts also receive copies in their resolved configuration directory's `skills` folder before a provider launches. Later Apply and Stop sharing operations include those account copies. Removing an account retires its export targets before deleting its profile.

An unmanaged destination or an externally edited copy appears as a conflict. MonoCode preserves it. Open that location to review the copy. Repair sharing restores missing managed copies and retries export failures. It does not overwrite conflicts. Stop sharing removes unchanged managed copies and keeps unmanaged or edited copies.

The applied preview shows the saved revision. Editing the source does not change that preview until you apply edits. Settings follows the selected project when it discovers existing skills.

Skill library errors appear in Settings and the provider launch log. Ordinary provider launches continue. Account removal stops if MonoCode cannot retire the account's export records.

Provider statuses describe files on this host. They do not prove that a provider has loaded a skill or that its scripts and tools are available. Antigravity has no default export until its personal runtime location is verified. Remote hosts keep their own skill directories. This version manages local exports.

Pi and OMP composer catalogs include their native commands and discovered file skills. A file skill with a command name collision has a qualified invocation such as `/skill:review`. The picker also qualifies file skills when native discovery fails. Native command arguments retain their original text. File skill prompts include the instruction file and resource directory so providers can locate bundled scripts and references.

## Preview

Run the real manager with disposable app data and example skills:

```sh
scripts/preview-skill-manager.sh
```

The script imports two complete bundles and creates an independent Claude copy to demonstrate conflict preservation. It prints the fixture directory. It does not use personal skill directories or start provider sessions.

For an existing screenshot build, supply its path and capture the real view:

```sh
MONOCODE_PREVIEW_BINARY=/path/to/monocode-app \
  scripts/preview-skill-manager.sh --size 1280x800 --screenshot /tmp/skill-manager.png
```

`--prepare-only` creates the disposable library without opening the app. `MONOCODE_PREVIEW_LIBRARY_TARGET` selects the lightweight service build cache. The app also accepts `--skills-home` with an absolute directory. Use it together with a separate `--data-dir` for isolated development runs.

## Validation

The shared skills workflow runs library tests, clippy, and process tests on Linux, macOS, and Windows. Its macOS job tests Pi command metadata, engine skill expansion, prompt handling, and submission behavior. It checks the native app and builds a downloadable preview binary.

The service tests cover complete bundles, executable modes, import deduplication, same-name import conflicts, ownership conflicts, interrupted updates, account retirement, and concurrent imports. See [the library README](../crates/skills/README.md) for bundle limits and storage behavior.
